//! Uji integrasi route `admin/jobs/*` dan `admin/national-holidays` (operator) terhadap
//! Postgres + Redis nyata (database `mz_test_it_adm_jobs_routes`, Redis DB test).
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-admin-jobs --test it_adm_jobs_routes`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use mz_core::AppState;
use mz_testkit::TestApp;
use serde_json::{Value, json};

const BUCKET: &str = "it-admin-backups";

fn build(state: &AppState) -> Router {
    mz_admin_jobs::router(state).with_state(state.clone())
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn key_path() -> String {
    let path = format!("{}/it_adm_jobs_sa.json", env!("CARGO_TARGET_TMPDIR"));
    std::fs::write(
        &path,
        json!({
            "client_email": "sa@test.iam.gserviceaccount.com",
            "private_key": include_str!("../../mz-files/src/test_key.pem"),
        })
        .to_string(),
    )
    .unwrap();
    path
}

async fn app() -> Option<TestApp> {
    let key = key_path();
    TestApp::with_vars(
        build,
        &[("ADMIN_BACKUP_BUCKET", BUCKET), ("GCP_KEY_PATH", &key)],
    )
    .await
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
        .bind(format!("it-adm-jobs-{}", uniq()))
        .fetch_one(app.db())
        .await
        .unwrap()
}

async fn audit_rows(
    app: &TestApp,
    action: &str,
    target: &str,
) -> Vec<(Option<Value>, Option<Value>, Option<String>, Option<i32>)> {
    sqlx::query_as(
        r#"SELECT before, after, reason, "companyId" FROM "operatorAuditLog"
            WHERE action = $1 AND "targetUuid" = $2::uuid ORDER BY id"#,
    )
    .bind(action)
    .bind(target)
    .fetch_all(app.db())
    .await
    .unwrap()
}

#[tokio::test]
async fn gerbang_operator_dan_role() {
    let Some(app) = app().await else { return };
    let (_, support) = operator(&app, "SUPPORT").await;
    let (_, finance) = operator(&app, "FINANCE").await;
    let fx = app.company().await;

    for (method, uri) in [
        ("POST", "/admin/jobs/backup"),
        ("GET", "/admin/jobs"),
        ("GET", "/admin/jobs/11111111-1111-4111-8111-111111111111"),
        ("GET", "/admin/national-holidays?year=2027"),
        ("POST", "/admin/national-holidays"),
    ] {
        let m = http::Method::from_bytes(method.as_bytes()).unwrap();
        let res = app.request(m.clone(), uri).send().await;
        res.assert_status(401);
        assert_eq!(
            res.json,
            json!({ "message": "Unauthorized", "statusCode": 401 })
        );
        // Token user biasa (bukan operator) → 401.
        app.request(m, uri)
            .user(fx.super_admin.id)
            .send()
            .await
            .assert_status(401);
    }

    for uri in [
        "/admin/jobs/backup",
        "/admin/jobs/restore",
        "/admin/jobs/merge",
    ] {
        let res = app.post(uri).token(&support).json(&json!({})).send().await;
        res.assert_status(403);
        assert_eq!(res.json["code"], "OPERATOR_ROLE_FORBIDDEN");
    }
    let res = app
        .get("/admin/jobs/11111111-1111-4111-8111-111111111111/download")
        .token(&finance)
        .send()
        .await;
    res.assert_status(403);
    let res = app
        .post("/admin/national-holidays")
        .token(&finance)
        .json(&json!({}))
        .send()
        .await;
    res.assert_status(403);
    // GET daftar boleh untuk ketiga role.
    app.get("/admin/jobs")
        .token(&finance)
        .send()
        .await
        .assert_status(200)
        .assert_no_sql_error();
    // GET /admin/jobs/backup jatuh ke `:uuid` (ParseUUIDPipe) seperti Nest.
    let res = app.get("/admin/jobs/backup").token(&support).send().await;
    res.assert_status(400);
    assert_eq!(res.json["message"], "Validation failed (uuid is expected)");
}

#[tokio::test]
async fn backup_antre_audit_dan_satu_job_aktif() {
    let Some(app) = app().await else { return };
    let (op_id, owner) = operator(&app, "OWNER").await;
    let (company_id, company_uuid) = company(&app).await;

    // Validasi (urutan pesan sama dengan Node).
    let res = app
        .post("/admin/jobs/backup")
        .token(&owner)
        .json(&json!({ "companyUuid": "x", "extra": 1 }))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "companyUuid must be a UUID",
            "reason must be longer than or equal to 5 characters",
            "reason must be a string"
        ])
    );
    // Company tak dikenal → 404.
    let res = app
        .post("/admin/jobs/backup")
        .token(&owner)
        .json(&json!({ "companyUuid": uuid::Uuid::new_v4().to_string(), "reason": "Backup rutin" }))
        .send()
        .await;
    res.assert_status(404);
    assert_eq!(
        res.json,
        json!({ "code": "COMPANY_NOT_FOUND", "message": "Company not found" })
    );

    let res = app
        .post("/admin/jobs/backup")
        .token(&owner)
        .header("user-agent", "it-agent")
        .json(&json!({ "companyUuid": company_uuid, "skipHeavy": true, "reason": "  Backup rutin  " }))
        .send()
        .await;
    res.assert_status(202).assert_no_sql_error();
    assert_eq!(res.json["data"]["status"], "QUEUED");
    let job = res.json["data"]["jobUuid"].as_str().unwrap().to_owned();

    let (ty, status, cid, params, dry, op): (String, String, Option<i32>, Value, bool, i32) =
        sqlx::query_as(
            r#"SELECT type::text, status::text, "companyId", params, "dryRun", "operatorId"
                 FROM "adminJob" WHERE uuid = $1::uuid"#,
        )
        .bind(&job)
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(
        (ty.as_str(), status.as_str(), cid, dry, op),
        ("BACKUP_COMPANY", "QUEUED", Some(company_id), false, op_id)
    );
    assert_eq!(
        params,
        json!({ "companyUuid": company_uuid, "skipHeavy": true, "redactSecrets": false })
    );
    let audit = audit_rows(&app, "admin_job.create", &job).await;
    assert_eq!(audit.len(), 1);
    assert!(audit[0].0.is_none());
    assert_eq!(audit[0].1.as_ref().unwrap()["type"], "BACKUP_COMPANY");
    assert_eq!(audit[0].1.as_ref().unwrap()["runner"], "rust");
    assert_eq!(audit[0].2.as_deref(), Some("Backup rutin"));
    assert_eq!(audit[0].3, Some(company_id));
    // Antrean Rust berisi job ini.
    let queued = mz_core::queue::get(app.state(), "admin-jobs", &format!("admin-job:{job}"))
        .await
        .unwrap();
    assert_eq!(queued.unwrap().data, json!({ "uuid": job }));

    // Job kedua untuk company yang sama → 409 (kunci & cek yang sama dengan Node).
    let res = app
        .post("/admin/jobs/backup")
        .token(&owner)
        .json(&json!({ "companyUuid": company_uuid, "reason": "Backup rutin" }))
        .send()
        .await;
    res.assert_status(409);
    assert_eq!(
        res.json,
        json!({ "code": "JOB_ALREADY_ACTIVE", "message": "Another admin job for this company is still queued or running" })
    );
    // Job aktif buatan Node (tanpa penanda runner) juga memblokir.
    let (_, other_uuid) = company(&app).await;
    app.exec(&format!(
        r#"INSERT INTO "adminJob" (type, "companyId", params, "paramsHash", "dryRun", status, "operatorId")
           SELECT 'MERGE_COMPANY', id, '{{}}', 'x', true, 'RUNNING', {op_id} FROM company WHERE uuid = '{other_uuid}'"#
    ))
    .await;
    let res = app
        .post("/admin/jobs/backup")
        .token(&owner)
        .json(&json!({ "companyUuid": other_uuid, "reason": "Backup rutin" }))
        .send()
        .await;
    res.assert_status(409);
    let _ = mz_core::queue::ack(app.state(), "admin-jobs", &format!("admin-job:{job}")).await;
}

#[tokio::test]
async fn restore_dan_merge_prasyarat() {
    let Some(app) = app().await else { return };
    let (op_id, owner) = operator(&app, "OWNER").await;
    let (company_id, company_uuid) = company(&app).await;

    let base = json!({
        "backupJobUuid": uuid::Uuid::new_v4().to_string(), "mode": "clone", "users": "insert",
        "dryRun": true, "reason": "Uji restore"
    });
    let res = app
        .post("/admin/jobs/restore")
        .token(&owner)
        .json(&base)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(res.json["code"], "USERNAME_SUFFIX_REQUIRED");

    let mut body = base.clone();
    body["users"] = json!("reuse");
    let res = app
        .post("/admin/jobs/restore")
        .token(&owner)
        .json(&body)
        .send()
        .await;
    res.assert_status(404);
    assert_eq!(
        res.json,
        json!({ "code": "BACKUP_JOB_NOT_FOUND", "message": "Backup job not found" })
    );

    // Backup belum DONE → 409.
    let backup: String = sqlx::query_scalar(
        r#"INSERT INTO "adminJob" (type, "companyId", params, "paramsHash", "dryRun", status, "operatorId", "finishedAt")
           VALUES ('BACKUP_COMPANY', $1, '{}', 'h', false, 'FAILED', $2, NOW()) RETURNING uuid::text"#,
    )
    .bind(company_id)
    .bind(op_id)
    .fetch_one(app.db())
    .await
    .unwrap();
    body["backupJobUuid"] = json!(backup);
    let res = app
        .post("/admin/jobs/restore")
        .token(&owner)
        .json(&body)
        .send()
        .await;
    res.assert_status(409);
    assert_eq!(res.json["code"], "BACKUP_NOT_AVAILABLE");

    // Backup DONE: dry run diantrekan; run sungguhan tanpa dry run DONE → 409 DRY_RUN_REQUIRED.
    sqlx::query(
        r#"UPDATE "adminJob" SET status = 'DONE', result = $2 WHERE uuid = $1::uuid"#,
    )
    .bind(&backup)
    .bind(json!({ "bucket": BUCKET, "prefix": "company-backups/x/", "files": [{ "name": "manifest.json", "size": 2 }] }))
    .execute(app.db())
    .await
    .unwrap();
    let mut real = body.clone();
    real["dryRun"] = json!(false);
    let res = app
        .post("/admin/jobs/restore")
        .token(&owner)
        .json(&real)
        .send()
        .await;
    res.assert_status(409);
    assert_eq!(
        res.json,
        json!({ "code": "DRY_RUN_REQUIRED", "message": "A successful dry run with the same parameters within the last 24 hours is required" })
    );
    let res = app
        .post("/admin/jobs/restore")
        .token(&owner)
        .json(&body)
        .send()
        .await;
    res.assert_status(202).assert_no_sql_error();
    let dry_job = res.json["data"]["jobUuid"].as_str().unwrap().to_owned();
    let params: Value = app
        .scalar(&format!(
            r#"SELECT params FROM "adminJob" WHERE uuid = '{dry_job}'"#
        ))
        .await;
    assert_eq!(
        params,
        json!({ "backupJobUuid": backup, "mode": "clone", "rename": null, "users": "reuse", "usernameSuffix": null })
    );
    // Dry run DONE dengan hash yang sama → run sungguhan diterima.
    app.exec(&format!(
        r#"UPDATE "adminJob" SET status = 'DONE', "finishedAt" = NOW() WHERE uuid = '{dry_job}'"#
    ))
    .await;
    let res = app
        .post("/admin/jobs/restore")
        .token(&owner)
        .json(&real)
        .send()
        .await;
    res.assert_status(202);
    let real_job = res.json["data"]["jobUuid"].as_str().unwrap().to_owned();
    let dry_flag: bool = app
        .scalar(&format!(
            r#"SELECT "dryRun" FROM "adminJob" WHERE uuid = '{real_job}'"#
        ))
        .await;
    assert!(!dry_flag);
    for j in [&dry_job, &real_job] {
        let _ = mz_core::queue::ack(app.state(), "admin-jobs", &format!("admin-job:{j}")).await;
    }

    // Merge tanpa LEGACY_DATABASE_URL → 412; rentang terbalik dicek sesudahnya.
    let merge = json!({
        "targetCompanyUuid": company_uuid, "legacyCompanyId": 4, "retention": "ACCEPT",
        "dryRun": true, "reason": "Merge data lama"
    });
    let res = app
        .post("/admin/jobs/merge")
        .token(&owner)
        .json(&merge)
        .send()
        .await;
    res.assert_status(412);
    assert_eq!(res.json["code"], "LEGACY_DATABASE_NOT_CONFIGURED");
    let url = app.state().config.database_url.clone();
    let app2 = TestApp::with_vars(build, &[("LEGACY_DATABASE_URL", &url)])
        .await
        .unwrap();
    let mut bad = merge.clone();
    bad["since"] = json!("2026-10-12");
    bad["until"] = json!("2026-10-01");
    let res = app2
        .post("/admin/jobs/merge")
        .token(&owner)
        .json(&bad)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(res.json["code"], "INVALID_DATE_RANGE");
    // Bucket tidak diset → backup/restore 412 (merge tidak butuh bucket).
    let res = app2
        .post("/admin/jobs/backup")
        .token(&owner)
        .json(&json!({ "companyUuid": company_uuid, "reason": "Backup rutin" }))
        .send()
        .await;
    res.assert_status(412);
    assert_eq!(res.json["code"], "ADMIN_BACKUP_BUCKET_NOT_CONFIGURED");
}

#[tokio::test]
async fn list_detail_download() {
    let Some(app) = app().await else { return };
    let (op_id, owner) = operator(&app, "OWNER").await;
    let (_, support) = operator(&app, "SUPPORT").await;
    let (company_id, company_uuid) = company(&app).await;
    let done: String = sqlx::query_scalar(
        r#"INSERT INTO "adminJob" (type, "companyId", params, "paramsHash", "dryRun", status, "operatorId",
                                   result, log, "startedAt", "finishedAt")
           VALUES ('BACKUP_COMPANY', $1, '{"skipHeavy": false}', 'h', false, 'DONE', $2, $3, 'baris log', NOW(), NOW())
           RETURNING uuid::text"#,
    )
    .bind(company_id)
    .bind(op_id)
    .bind(json!({
        "bucket": BUCKET, "prefix": "company-backups/x/",
        "files": [{ "name": "manifest.json", "size": 10 }, { "name": "data/users.ndjson.gz", "size": 20 }]
    }))
    .fetch_one(app.db())
    .await
    .unwrap();

    // Daftar dengan filter company.
    let res = app
        .get(&format!(
            "/admin/jobs?companyUuid={company_uuid}&type=BACKUP_COMPANY&pageNumber=1&itemsPerPage=5"
        ))
        .token(&support)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(res.json["total"], 1);
    assert_eq!(res.json["pageNumber"], 1);
    assert_eq!(res.json["itemsPerPage"], 5);
    let row = &res.json["data"][0];
    let keys: Vec<&str> = row
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "uuid",
            "type",
            "status",
            "dryRun",
            "params",
            "createdAt",
            "startedAt",
            "finishedAt",
            "operator",
            "company"
        ]
    );
    assert_eq!(row["company"]["uuid"], company_uuid);
    assert_eq!(row["operator"]["name"], "IT Operator");
    assert!(row["createdAt"].as_str().unwrap().ends_with('Z'));
    let res = app
        .get("/admin/jobs?type=X&foo=1")
        .token(&support)
        .send()
        .await;
    res.assert_status(400);
    assert!(!res.json["message"].to_string().contains("property foo"));
    // uuid company tak dikenal → hasil kosong.
    let res = app
        .get(&format!("/admin/jobs?companyUuid={}", uuid::Uuid::new_v4()))
        .token(&support)
        .send()
        .await;
    assert_eq!(res.json["total"], 0);

    // Detail.
    let res = app
        .get(&format!("/admin/jobs/{done}"))
        .token(&support)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(res.json["data"]["log"], "baris log");
    assert_eq!(res.json["data"]["paramsHash"], "h");
    assert_eq!(res.json["data"]["result"]["prefix"], "company-backups/x/");
    let res = app
        .get("/admin/jobs/bukan-uuid")
        .token(&support)
        .send()
        .await;
    res.assert_status(400);
    let res = app
        .get(&format!("/admin/jobs/{}", uuid::Uuid::new_v4()))
        .token(&support)
        .send()
        .await;
    res.assert_status(404);
    assert_eq!(
        res.json,
        json!({ "code": "ADMIN_JOB_NOT_FOUND", "message": "Admin job not found" })
    );

    // Download: signed URL V4 per file, 900 detik, dari bucket di result.
    let res = app
        .get(&format!("/admin/jobs/{done}/download"))
        .token(&owner)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    let files = res.json["data"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[1]["name"], "data/users.ndjson.gz");
    let url = files[1]["url"].as_str().unwrap();
    assert!(
        url.contains(&format!("{BUCKET}/company-backups/x/data/users.ndjson.gz")),
        "{url}"
    );
    assert!(url.contains("X-Goog-Expires=900"), "{url}");
    // Bukan backup DONE → 409.
    let restore: String = app
        .scalar(&format!(
            r#"INSERT INTO "adminJob" (type, "companyId", params, "paramsHash", "dryRun", status, "operatorId")
               VALUES ('RESTORE_COMPANY', NULL, '{{}}', 'h', true, 'DONE', {op_id}) RETURNING uuid::text"#
        ))
        .await;
    let res = app
        .get(&format!("/admin/jobs/{restore}/download"))
        .token(&owner)
        .send()
        .await;
    res.assert_status(409);
    assert_eq!(
        res.json,
        json!({ "code": "JOB_NOT_DOWNLOADABLE", "message": "Only finished backup jobs can be downloaded" })
    );
}

#[tokio::test]
async fn libur_nasional_crud() {
    let Some(app) = app().await else { return };
    let (_, owner) = operator(&app, "OWNER").await;
    let (_, finance) = operator(&app, "FINANCE").await;
    let name = format!("Libur {}", uniq());

    let res = app
        .get("/admin/national-holidays")
        .token(&finance)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "year must not be greater than 2100",
            "year must not be less than 2000",
            "year must be an integer number"
        ])
    );

    let res = app
        .post("/admin/national-holidays")
        .token(&owner)
        .json(&json!({ "date": "2099-02-30", "name": name, "type": "NATIONAL_HOLIDAY", "isEstimated": false }))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json,
        json!({ "code": "INVALID_DATE", "message": "date is not a valid calendar date" })
    );

    let res = app
        .post("/admin/national-holidays")
        .token(&owner)
        .json(&json!({ "date": "2099-03-10", "name": format!(" {name} "), "type": "JOINT_LEAVE", "isEstimated": true, "reason": "Tambah libur" }))
        .send()
        .await;
    res.assert_status(201).assert_no_sql_error();
    let data = res.json["data"].clone();
    let keys: Vec<&str> = data
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["uuid", "date", "name", "type", "year", "isEstimated"]
    );
    assert_eq!(data["date"], "2099-03-10");
    assert_eq!(data["name"], name);
    assert_eq!(data["year"], 2099);
    let uuid = data["uuid"].as_str().unwrap().to_owned();
    let audit = audit_rows(&app, "national_holiday.create", &uuid).await;
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].1.as_ref(), Some(&data));
    assert_eq!(audit[0].2.as_deref(), Some("Tambah libur"));

    let dup = app
        .post("/admin/national-holidays")
        .token(&owner)
        .json(&json!({ "date": "2099-03-10", "name": name, "type": "NATIONAL_HOLIDAY", "isEstimated": false }))
        .send()
        .await;
    dup.assert_status(409);
    assert_eq!(dup.json["code"], "DUPLICATE_HOLIDAY");

    let res = app
        .get("/admin/national-holidays?year=2099")
        .token(&finance)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert!(
        res.json["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["uuid"] == uuid.as_str())
    );

    let res = app
        .patch(&format!("/admin/national-holidays/{uuid}"))
        .token(&owner)
        .json(&json!({ "date": "2099-03-11", "isEstimated": false }))
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(res.json["data"]["date"], "2099-03-11");
    assert_eq!(res.json["data"]["isEstimated"], false);
    assert_eq!(res.json["data"]["type"], "JOINT_LEAVE");
    let audit = audit_rows(&app, "national_holiday.update", &uuid).await;
    assert_eq!(audit[0].0.as_ref().unwrap()["date"], "2099-03-10");
    assert_eq!(audit[0].1.as_ref().unwrap()["date"], "2099-03-11");

    let missing = uuid::Uuid::new_v4();
    let res = app
        .patch(&format!("/admin/national-holidays/{missing}"))
        .token(&owner)
        .json(&json!({ "name": "x" }))
        .send()
        .await;
    res.assert_status(404);
    assert_eq!(
        res.json,
        json!({ "code": "HOLIDAY_NOT_FOUND", "message": "national holiday not found" })
    );
    let res = app
        .patch("/admin/national-holidays/apply")
        .token(&owner)
        .json(&json!({}))
        .send()
        .await;
    res.assert_status(400);

    let res = app
        .delete(&format!("/admin/national-holidays/{uuid}"))
        .token(&owner)
        .json(&json!({ "reason": "ab" }))
        .send()
        .await;
    res.assert_status(400);
    let res = app
        .delete(&format!("/admin/national-holidays/{uuid}"))
        .token(&owner)
        .json(&json!({ "reason": "Hapus duplikat" }))
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(res.json, json!({ "data": { "deleted": true } }));
    let audit = audit_rows(&app, "national_holiday.delete", &uuid).await;
    assert_eq!(audit.len(), 1);
    assert!(audit[0].1.is_none());
    assert_eq!(audit[0].2.as_deref(), Some("Hapus duplikat"));
    let left: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "nationalHoliday" WHERE uuid = '{uuid}'"#
        ))
        .await;
    assert_eq!(left, 0);
}
