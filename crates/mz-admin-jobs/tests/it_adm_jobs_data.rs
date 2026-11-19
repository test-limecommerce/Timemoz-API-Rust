//! Uji integrasi worker `admin-jobs` (backup → restore dry run → restore, merge dari "DB lama",
//! reaper) terhadap Postgres nyata (`mz_test_it_adm_jobs_data`, data dump dianonimkan).
//! GCS = server tiruan lokal (`STORAGE_EMULATOR_HOST`); DB lama merge = database test itu
//! sendiri (merge idempoten terhadap dirinya). Tidak pernah menyentuh database lain.
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-admin-jobs --test it_adm_jobs_data`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use mz_admin_jobs::jobs::worker::{process, reap_stale_jobs};
use mz_core::AppState;
use mz_testkit::TestApp;
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;

const BUCKET: &str = "it-admin-backups";
/// Satu job aktif per company + satu database: test di binary ini dijalankan berurutan.
static SERIAL: AsyncMutex<()> = AsyncMutex::const_new(());

type Store = Arc<Mutex<HashMap<String, Vec<u8>>>>;

fn build(state: &AppState) -> Router {
    mz_admin_jobs::router(state).with_state(state.clone())
}

/// GCS JSON API tiruan: upload media + download `alt=media`.
async fn fake_gcs() -> (String, Store) {
    let store: Store = Arc::default();
    async fn upload(
        State(s): State<Store>,
        Path(bucket): Path<String>,
        Query(q): Query<HashMap<String, String>>,
        body: axum::body::Bytes,
    ) -> axum::Json<Value> {
        let name = q.get("name").cloned().unwrap_or_default();
        s.lock()
            .unwrap()
            .insert(format!("{bucket}/{name}"), body.to_vec());
        axum::Json(json!({ "name": name }))
    }
    async fn download(
        State(s): State<Store>,
        Path((bucket, key)): Path<(String, String)>,
    ) -> Result<Vec<u8>, http::StatusCode> {
        s.lock()
            .unwrap()
            .get(&format!("{bucket}/{key}"))
            .cloned()
            .ok_or(http::StatusCode::NOT_FOUND)
    }
    let app = Router::new()
        .route("/upload/storage/v1/b/{bucket}/o", post(upload))
        .route("/storage/v1/b/{bucket}/o/{key}", get(download))
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(store.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), store)
}

/// Database "Timemoz lama" tiruan: salinan template test dengan kolom skema lama
/// (`company.dailyReportSentAt` masih `timestamp`, seperti mudah-api-old). Hanya dibuat dari
/// template test di server test; dibuang di awal tiap run.
async fn legacy_db(test_url: &str) -> String {
    use sqlx::Connection;
    let name = "mz_test_it_adm_jobs_data_legacy";
    let template =
        std::env::var("MZ_TEST_TEMPLATE").unwrap_or_else(|_| "mudah_test_template".into());
    let swap = |db: &str| {
        let (base, query) = test_url
            .split_once('?')
            .map_or((test_url, None), |(b, q)| (b, Some(q)));
        let root = base.rsplit_once('/').map_or(base, |(r, _)| r);
        match query {
            Some(q) => format!("{root}/{db}?{q}"),
            None => format!("{root}/{db}"),
        }
    };
    let mut admin = sqlx::PgConnection::connect(&swap("postgres"))
        .await
        .unwrap();
    sqlx::query(&format!(r#"DROP DATABASE IF EXISTS "{name}" WITH (FORCE)"#))
        .execute(&mut admin)
        .await
        .unwrap();
    let mut last = String::new();
    for attempt in 0..40u64 {
        match sqlx::query(&format!(
            r#"CREATE DATABASE "{name}" TEMPLATE "{template}" STRATEGY FILE_COPY"#
        ))
        .execute(&mut admin)
        .await
        {
            Ok(_) => {
                last.clear();
                break;
            }
            Err(e) => {
                last = e.to_string();
                tokio::time::sleep(std::time::Duration::from_millis(200 + attempt * 100)).await;
            }
        }
    }
    assert!(last.is_empty(), "create legacy db: {last}");
    let url = swap(name);
    let mut conn = sqlx::PgConnection::connect(&url).await.unwrap();
    sqlx::raw_sql(
        r#"ALTER TABLE company ALTER COLUMN "dailyReportSentAt" DROP DEFAULT;
           ALTER TABLE company ALTER COLUMN "dailyReportSentAt" TYPE timestamp(3)
             USING (DATE '1970-01-01' + "dailyReportSentAt")"#,
    )
    .execute(&mut conn)
    .await
    .unwrap();
    url
}

async fn setup(with_legacy: bool) -> Option<(TestApp, Store)> {
    let (gcs, store) = fake_gcs().await;
    let probe = TestApp::new(build).await?;
    let url = if with_legacy {
        legacy_db(&probe.state().config.database_url).await
    } else {
        String::new()
    };
    let app = TestApp::with_vars(
        build,
        &[
            ("ADMIN_BACKUP_BUCKET", BUCKET),
            ("STORAGE_EMULATOR_HOST", &gcs),
            ("LEGACY_DATABASE_URL", &url),
        ],
    )
    .await?;
    Some((app, store))
}

async fn owner(app: &TestApp) -> String {
    let u = uuid::Uuid::new_v4().simple().to_string();
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO operators (username, password, name, status, role)
               VALUES ('op{u}', 'x', 'IT Owner', 1, 'OWNER') RETURNING id"#
        ))
        .await;
    app.token_with(json!({ "sub": id }))
}

async fn job_row(app: &TestApp, uuid: &str) -> (String, Option<Value>, String) {
    sqlx::query_as(
        r#"SELECT status::text, result, COALESCE(log, '') FROM "adminJob" WHERE uuid = $1::uuid"#,
    )
    .bind(uuid)
    .fetch_one(app.db())
    .await
    .unwrap()
}

async fn create(app: &TestApp, token: &str, uri: &str, body: Value) -> String {
    let res = app.post(uri).token(token).json(&body).send().await;
    res.assert_status(202);
    let uuid = res.json["data"]["jobUuid"].as_str().unwrap().to_owned();
    // Worker antrean tidak dijalankan di test (Redis dipakai bersama binary lain): job diproses
    // langsung, entri antreannya dibuang.
    let _ = mz_core::queue::ack(app.state(), "admin-jobs", &format!("admin-job:{uuid}")).await;
    uuid
}

async fn run(app: &TestApp, uuid: &str) -> (String, Value, String) {
    process(app.state(), uuid).await;
    let (status, result, log) = job_row(app, uuid).await;
    (status, result.unwrap_or(Value::Null), log)
}

async fn count(app: &TestApp, sql: &str) -> i64 {
    app.scalar::<i64>(sql).await
}

#[tokio::test]
async fn backup_lalu_restore_clone() {
    let _g = SERIAL.lock().await;
    let Some((app, store)) = setup(false).await else {
        return;
    };
    let token = owner(&app).await;
    let (company_id, company_uuid): (i32, String) =
        sqlx::query_as("SELECT id, uuid::text FROM company WHERE id = 3")
            .fetch_one(app.db())
            .await
            .unwrap();

    // ── Backup ──
    let backup = create(
        &app,
        &token,
        "/admin/jobs/backup",
        json!({ "companyUuid": company_uuid, "redactSecrets": true, "reason": "Backup uji integrasi" }),
    )
    .await;
    let (status, result, log) = run(&app, &backup).await;
    assert_eq!(status, "DONE", "{log}");
    assert!(log.contains("Start BACKUP_COMPANY"), "{log}");
    assert!(log.contains("Done BACKUP_COMPANY"), "{log}");
    assert_eq!(result["bucket"], BUCKET);
    let prefix = format!("company-backups/{backup}/");
    assert_eq!(result["prefix"], prefix.as_str());
    let files = result["files"].as_array().unwrap();
    assert!(files.iter().any(|f| f["name"] == "manifest.json"));
    assert!(files.iter().any(|f| f["name"] == "data/company.ndjson.gz"));
    assert_eq!(result["manifest"]["source"]["companyId"], company_id);
    assert_eq!(result["manifest"]["options"]["redactSecrets"], true);
    let golden: Value = serde_json::from_str(include_str!("golden/scope_plan.json")).unwrap();
    let manifest_bytes = store
        .lock()
        .unwrap()
        .get(&format!("{BUCKET}/{prefix}manifest.json"))
        .cloned()
        .unwrap();
    let manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    // Sidik jari schema = Node: backup Rust bisa direstore Node dan sebaliknya.
    assert_eq!(manifest["schemaFingerprint"], golden["fingerprint"]);
    assert_eq!(manifest["formatVersion"], 2);
    assert_eq!(manifest["order"], golden["plans"]["{}"]["order"]);
    let users_in_backup = manifest["tables"]["users"]["rows"].as_i64().unwrap();
    let uc_in_backup = manifest["tables"]["userCompany"]["rows"].as_i64().unwrap();
    assert_eq!(
        uc_in_backup,
        count(
            &app,
            &format!(r#"SELECT COUNT(*) FROM "userCompany" WHERE "companyId" = {company_id}"#)
        )
        .await
    );
    assert!(users_in_backup > 0);
    let finish: i64 = count(
        &app,
        &format!(r#"SELECT COUNT(*) FROM "operatorAuditLog" WHERE action = 'admin_job.finish' AND "targetUuid" = '{backup}'"#),
    )
    .await;
    assert_eq!(finish, 1);
    // Job yang sudah DONE tidak dijalankan ulang.
    process(app.state(), &backup).await;
    assert_eq!(job_row(&app, &backup).await.0, "DONE");

    // ── Restore dry run: semua ditulis lalu di-rollback ──
    let companies_before = count(&app, "SELECT COUNT(*) FROM company").await;
    let users_before = count(&app, "SELECT COUNT(*) FROM users").await;
    let body = json!({
        "backupJobUuid": backup, "mode": "clone", "users": "insert", "usernameSuffix": ".copy",
        "rename": "PT Salinan Uji", "dryRun": true, "reason": "Uji restore"
    });
    let dry = create(&app, &token, "/admin/jobs/restore", body.clone()).await;
    let (status, result, log) = run(&app, &dry).await;
    assert_eq!(status, "DONE", "{log}");
    assert_eq!(result["status"], "dry-run");
    assert_eq!(result["dryRun"], true);
    assert_eq!(result["backupJobUuid"], backup.as_str());
    assert!(result["inserted"].as_i64().unwrap() > 0);
    assert!(result["newCompanyId"].as_i64().is_some());
    assert!(log.contains("DRY RUN selesai"), "{log}");
    assert_eq!(
        count(&app, "SELECT COUNT(*) FROM company").await,
        companies_before
    );
    assert_eq!(
        count(&app, "SELECT COUNT(*) FROM users").await,
        users_before
    );

    // ── Restore sungguhan (dry run DONE dengan params sama sudah ada) ──
    let mut real_body = body.clone();
    real_body["dryRun"] = json!(false);
    let real = create(&app, &token, "/admin/jobs/restore", real_body).await;
    let (status, result, log) = run(&app, &real).await;
    assert_eq!(status, "DONE", "{log}");
    assert_eq!(result["status"], "done");
    let new_id = result["newCompanyId"].as_i64().unwrap();
    assert_ne!(new_id, i64::from(company_id));
    let (name, new_uuid): (String, String) =
        sqlx::query_as("SELECT name, uuid::text FROM company WHERE id = $1")
            .bind(i32::try_from(new_id).unwrap())
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(name, "PT Salinan Uji");
    assert_ne!(new_uuid, company_uuid);
    assert_eq!(
        count(
            &app,
            &format!(r#"SELECT COUNT(*) FROM "userCompany" WHERE "companyId" = {new_id}"#)
        )
        .await,
        uc_in_backup
    );
    assert_eq!(
        count(&app, "SELECT COUNT(*) FROM users").await,
        users_before + result["tables"]["users"]["inserted"].as_i64().unwrap()
    );
    // Users salinan memakai akhiran dan password yang di-redact.
    let copies = count(
        &app,
        &format!(
            r#"SELECT COUNT(*) FROM users u JOIN "userCompany" uc ON uc."userId" = u.id
                WHERE uc."companyId" = {new_id} AND u.username LIKE '%.copy' AND u.password = '[REDACTED]'"#
        ),
    )
    .await;
    assert!(copies > 0);
    // Rujukan tidak pernah menunjuk ke baris company asal.
    assert_eq!(
        count(
            &app,
            &format!(
                r#"SELECT COUNT(*) FROM "projectCompany" pc WHERE pc."companyId" = {new_id}
                     AND pc."projectId" IN (SELECT "projectId" FROM "projectCompany" WHERE "companyId" = {company_id})"#
            ),
        )
        .await,
        0
    );
    let audit_company = count(
        &app,
        &format!(r#"SELECT COUNT(*) FROM "auditLog" WHERE "companyId" = {new_id} AND "entityType" = 'company'"#),
    )
    .await;
    assert!(
        audit_company >= 1,
        "createMany company tercatat seperti middleware Node"
    );
}

#[tokio::test]
async fn merge_dari_db_lama() {
    let _g = SERIAL.lock().await;
    let Some((app, _)) = setup(true).await else {
        return;
    };
    let token = owner(&app).await;
    let (company_uuid, other_uuid): (String, String) = sqlx::query_as(
        "SELECT (SELECT uuid::text FROM company WHERE id = 2), (SELECT uuid::text FROM company WHERE id = 3)",
    )
    .fetch_one(app.db())
    .await
    .unwrap();
    app.exec(r#"UPDATE company SET "screenshotRetentionDays" = 30 WHERE id = 2"#)
        .await;

    // Retensi terbatas + ACCEPT: dry run berjalan dan di-rollback.
    let body = json!({
        "targetCompanyUuid": company_uuid, "legacyCompanyId": 2, "retention": "ACCEPT",
        "since": "2020-01-01", "until": "2030-12-31", "dryRun": true, "reason": "Merge data lama"
    });
    let dry = create(&app, &token, "/admin/jobs/merge", body.clone()).await;
    let (status, result, log) = run(&app, &dry).await;
    assert_eq!(status, "DONE", "{log}");
    assert_eq!(result["committed"], false);
    assert_eq!(result["targetCompanyId"], 2);
    assert_eq!(result["legacyCompanyId"], 2);
    assert_eq!(result["retention"], json!({ "before": 30, "after": 30 }));
    // Sumber = tujuan: semua cocok lewat uuid, tidak ada yang disisipkan.
    assert_eq!(result["tables"]["users"]["inserted"], 0);
    assert_eq!(result["tables"]["company"]["byUuid"], 1);
    assert!(
        log.contains("DRY RUN — semua perubahan di-rollback"),
        "{log}"
    );
    assert!(
        log.contains("Hanya ada di tujuan (tidak disentuh): {\n  userCompany: "),
        "{log}"
    );

    // Apply butuh dry run DONE dengan params sama (sudah ada) → COMMIT; UNLIMITED beda params.
    let mut apply = body.clone();
    apply["dryRun"] = json!(false);
    let real = create(&app, &token, "/admin/jobs/merge", apply).await;
    let (status, result, log) = run(&app, &real).await;
    assert_eq!(status, "DONE", "{log}");
    assert_eq!(result["committed"], true);
    assert!(log.contains("COMMIT selesai."), "{log}");

    // Company tujuan lain dari uuid company lama → FAILED sebelum tulisan pertama.
    let mismatch = create(
        &app,
        &token,
        "/admin/jobs/merge",
        json!({
            "targetCompanyUuid": other_uuid, "legacyCompanyId": 2, "retention": "UNLIMITED",
            "dryRun": true, "reason": "Merge data lama"
        }),
    )
    .await;
    let (status, result, log) = run(&app, &mismatch).await;
    assert_eq!(status, "FAILED", "{log}");
    assert_eq!(
        result,
        json!({ "error": "Company lama cocok (uuid) dengan company #2 di tujuan, bukan #3 yang dipilih." })
    );
    let retention: i32 = app
        .scalar(r#"SELECT "screenshotRetentionDays" FROM company WHERE id = 2"#)
        .await;
    assert_eq!(retention, 30);
    // Salinan "DB lama" ±0,5 GB: buang (disk bersama).
    let _ =
        sqlx::query(r#"DROP DATABASE IF EXISTS "mz_test_it_adm_jobs_data_legacy" WITH (FORCE)"#)
            .execute(app.db())
            .await;
}

#[tokio::test]
async fn reaper_hanya_job_milik_rust() {
    let _g = SERIAL.lock().await;
    let Some((app, _)) = setup(false).await else {
        return;
    };
    let op: i32 = app
        .scalar("INSERT INTO operators (username, password, name, status, role) VALUES (gen_random_uuid()::text, 'x', 'IT', 1, 'OWNER') RETURNING id")
        .await;
    let mk = |status: &str, started: &str, created: &str, rust: bool| {
        format!(
            r#"WITH j AS (
                 INSERT INTO "adminJob" (type, "companyId", params, "paramsHash", "dryRun", status, "operatorId", "startedAt", "createdAt")
                 VALUES ('BACKUP_COMPANY', NULL, '{{}}', 'h', false, '{status}', {op}, {started}, {created}) RETURNING uuid)
               , a AS (INSERT INTO "operatorAuditLog" ("operatorId", action, "targetType", "targetUuid", after)
                 SELECT {op}, 'admin_job.create', 'adminJob', uuid, '{{"runner": "{}"}}'::jsonb FROM j WHERE {rust})
               SELECT uuid::text FROM j"#,
            if rust { "rust" } else { "node" }
        )
    };
    let old = "NOW() - interval '2 hours'";
    let rust_running: String = app.scalar(&mk("RUNNING", old, old, true)).await;
    let node_running: String = app.scalar(&mk("RUNNING", old, old, false)).await;
    let rust_queued_old: String = app
        .scalar(&mk("QUEUED", "NULL", "NOW() - interval '30 minutes'", true))
        .await;
    let rust_queued_new: String = app.scalar(&mk("QUEUED", "NULL", "NOW()", true)).await;

    let reaped = reap_stale_jobs(app.state(), mz_core::time::now_naive())
        .await
        .unwrap();
    assert!(reaped >= 2);
    let (s, _, log) = job_row(&app, &rust_running).await;
    assert_eq!(s, "FAILED");
    assert!(
        log.contains("Marked FAILED by boot reaper: worker restarted (job exceeded its timeout)")
    );
    assert_eq!(
        job_row(&app, &node_running).await.0,
        "RUNNING",
        "job Node tidak disentuh"
    );
    let (s, _, log) = job_row(&app, &rust_queued_old).await;
    assert_eq!(s, "FAILED");
    assert!(log.contains("queue job lost before it started"));
    assert_eq!(job_row(&app, &rust_queued_new).await.0, "QUEUED");
    let finish: Option<Value> = app
        .scalar(&format!(
            r#"SELECT after FROM "operatorAuditLog" WHERE action = 'admin_job.finish' AND "targetUuid" = '{rust_running}'"#
        ))
        .await;
    assert_eq!(
        finish.unwrap(),
        json!({ "type": "BACKUP_COMPANY", "status": "FAILED", "error": "worker restarted (job exceeded its timeout)" })
    );
    app.exec(&format!(
        r#"UPDATE "adminJob" SET status = 'FAILED' WHERE uuid IN ('{node_running}', '{rust_queued_new}')"#
    ))
    .await;
}

#[tokio::test]
async fn restore_backup_tanpa_file_gagal_dan_tercatat() {
    let _g = SERIAL.lock().await;
    let Some((app, _)) = setup(false).await else {
        return;
    };
    let op: i32 = app
        .scalar("INSERT INTO operators (username, password, name, status, role) VALUES (gen_random_uuid()::text, 'x', 'IT', 1, 'OWNER') RETURNING id")
        .await;
    // Baris RESTORE QUEUED yang menunjuk backup tak ada (mis. dihapus di sela) → FAILED + audit.
    let job: String = app
        .scalar(&format!(
            r#"INSERT INTO "adminJob" (type, "companyId", params, "paramsHash", "dryRun", status, "operatorId")
               VALUES ('RESTORE_COMPANY', NULL, '{{"mode": "clone", "users": "reuse", "backupJobUuid": "{}"}}', 'h', true, 'QUEUED', {op})
               RETURNING uuid::text"#,
            uuid::Uuid::new_v4()
        ))
        .await;
    let (status, result, log) = run(&app, &job).await;
    assert_eq!(status, "FAILED");
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("is not a finished backup")
    );
    assert!(log.contains("FAILED: Backup job"), "{log}");
    let after: Value = app
        .scalar(&format!(
            r#"SELECT after FROM "operatorAuditLog" WHERE action = 'admin_job.finish' AND "targetUuid" = '{job}'"#
        ))
        .await;
    assert_eq!(after["status"], "FAILED");
    assert_eq!(after["dryRun"], true);
}

// ── Kompatibilitas format dengan Node (opt-in, `--ignored`) ──────────────────────────────
//
// `MZ_ADM_COMPAT_DB_URL` = database salinan template test (BUKAN database lain);
// `MZ_ADM_NODE_BACKUP_DIR` = hasil `backupCompany` Node untuk company `MZ_ADM_COMPAT_COMPANY`
// di database yang sama. Test membandingkan isi setiap tabel baris-per-baris (sebagai JSON),
// lalu me-restore (dry run) backup Node dengan inti Rust. `MZ_ADM_RUST_BACKUP_OUT` menyimpan
// backup Rust untuk direstore Node.

fn read_ndjson(path: &std::path::Path) -> Vec<Value> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(path).unwrap();
    let r: Box<dyn std::io::Read> = if path.extension().is_some_and(|e| e == "gz") {
        Box::new(flate2::read::GzDecoder::new(f))
    } else {
        Box::new(f)
    };
    BufReader::new(r)
        .lines()
        .map(|l| l.unwrap())
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(&l).unwrap())
        .collect()
}

#[tokio::test]
#[ignore = "butuh MZ_ADM_COMPAT_DB_URL + backup Node"]
async fn kompatibel_dengan_backup_node() {
    use mz_admin_jobs::company_data::{backup, errors::Log, restore};
    struct Quiet;
    impl Log for Quiet {
        fn line(&self, _: &str) {}
    }
    let (Ok(db_url), Ok(node_dir)) = (
        std::env::var("MZ_ADM_COMPAT_DB_URL"),
        std::env::var("MZ_ADM_NODE_BACKUP_DIR"),
    ) else {
        return;
    };
    let company: i32 = std::env::var("MZ_ADM_COMPAT_COMPANY").map_or(3, |v| v.parse().unwrap());
    let pool = sqlx::PgPool::connect(&db_url).await.unwrap();
    let out = std::env::var("MZ_ADM_RUST_BACKUP_OUT").map_or_else(
        |_| std::env::temp_dir().join(format!("mz-adm-compat-{}", uuid::Uuid::new_v4())),
        std::path::PathBuf::from,
    );
    let _ = std::fs::remove_dir_all(&out);
    let mut conn = pool.acquire().await.unwrap();
    let manifest = backup::backup_company(
        &mut conn,
        &backup::BackupParams {
            company_id: company,
            out_dir: out.clone(),
            gzip: true,
            skip_heavy: false,
            redact_secrets: false,
            database_url: Some(db_url.clone()),
        },
        &Quiet,
    )
    .await
    .unwrap();
    drop(conn);
    let node_dir = std::path::PathBuf::from(node_dir);
    let node: Value =
        serde_json::from_str(&std::fs::read_to_string(node_dir.join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["schemaFingerprint"], node["schemaFingerprint"]);
    assert_eq!(manifest["order"], node["order"]);
    assert_eq!(manifest["totals"], node["totals"]);
    assert_eq!(manifest["externals"], node["externals"]);
    let mut compared = 0;
    for name in node["order"].as_array().unwrap() {
        let name = name.as_str().unwrap();
        let (n, r) = (&node["tables"][name], &manifest["tables"][name]);
        for k in [
            "rows",
            "droppedOutOfScope",
            "severedOptionalRefs",
            "danglingPolymorphicRefs",
        ] {
            assert_eq!(n[k], r[k], "{name}.{k}");
        }
        let a = read_ndjson(&node_dir.join(n["file"].as_str().unwrap()));
        let b = read_ndjson(&out.join(r["file"].as_str().unwrap()));
        assert_eq!(a.len(), b.len(), "{name}");
        // Bandingkan sebagai nilai JSON (urutan kunci tidak berarti).
        let mut a_vals: Vec<Value> = a.clone();
        let mut b_vals: Vec<Value> = b.clone();
        let sort_key = |v: &Value| {
            let mut m: Vec<(String, String)> = v
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, x)| (k.clone(), x.to_string()))
                .collect();
            m.sort();
            format!("{m:?}")
        };
        a_vals.sort_by_key(sort_key);
        b_vals.sort_by_key(sort_key);
        for (x, y) in a_vals.iter().zip(b_vals.iter()) {
            assert_eq!(x, y, "{name}");
        }
        compared += a.len();
    }
    eprintln!("baris dibandingkan: {compared}");

    // Restore (dry run) backup NODE dengan inti Rust.
    let res = restore::restore_company(
        &pool,
        &restore::RestoreParams {
            dir: node_dir,
            dry_run: true,
            users: restore::UsersMode::Insert,
            username_suffix: Some(".rustcopy".into()),
            rename: None,
            database_url: Some(db_url),
            lock_key: None,
        },
        &Quiet,
    )
    .await
    .unwrap();
    let inserted: serde_json::Map<String, Value> = res["tables"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, v)| v["inserted"].as_i64().unwrap_or(0) > 0)
        .map(|(k, v)| (k.clone(), v["inserted"].clone()))
        .collect();
    eprintln!(
        "RESTORE_RUST {}",
        json!({ "status": res["status"], "inserted": res["inserted"], "tables": inserted })
    );
    assert_eq!(res["status"], "dry-run");
}
