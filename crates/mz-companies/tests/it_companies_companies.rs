//! Uji integrasi 7 route `/companies*` lewat HTTP (gerbang lengkap + layer audit) terhadap
//! Postgres/Redis nyata. Jalankan:
//! `MZ_TEST_DATABASE_URL=... cargo test -p mz-companies --test it_companies_companies`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::Duration;

use axum::Router;
use fred::clients::SubscriberClient;
use fred::prelude::*;
use fred::types::Message;
use mz_core::AppState;
use mz_testkit::{Company, TestApp, TestResponse};
use serde_json::{Value, json};
use tokio::sync::Mutex;

/// Semua test memakai company fixture yang sama: dijalankan berurutan.
static SERIAL: Mutex<()> = Mutex::const_new(());

const FE: &str = "http://fe.test";
const SYSADMIN: &str = "it_companies_sysadmin";
const DUAL: &str = "it_companies_dual";

fn build(state: &AppState) -> Router {
    mz_companies::router(state)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

struct Ctx {
    app: TestApp,
    c: Company,
    channel: String,
    sysadmin: i32,
    /// Company lain (bukan fixture) yang aktif.
    other_uuid: String,
}

impl Ctx {
    fn sys(&self) -> String {
        self.app.token(self.sysadmin)
    }
}

async fn setup() -> Option<Ctx> {
    let channel = format!("mz:test:cache-invalidate:{}", uuid::Uuid::new_v4().simple());
    let key_path = format!("{}/it_companies_sa.json", env!("CARGO_TARGET_TMPDIR"));
    std::fs::write(
        &key_path,
        json!({
            "client_email": "sa@test.iam.gserviceaccount.com",
            "private_key": include_str!("../../mz-files/src/test_key.pem"),
        })
        .to_string(),
    )
    .unwrap();
    let app = TestApp::with_vars(
        build,
        &[
            ("CACHE_INVALIDATION_CHANNEL", &channel),
            ("FRONTEND_URL", FE),
            ("GCP_KEY_PATH", &key_path),
            ("GCP_BUCKET_NAME", "test-bucket"),
            ("MAX_SIZE_FILE_UPLOAD", "1000000"),
        ],
    )
    .await?;
    let c = app.company().await;
    let (other_id, other_uuid): (i32, String) = sqlx::query_as(
        "SELECT id, uuid::text FROM company WHERE status = 1 AND id <> $1 ORDER BY id LIMIT 1",
    )
    .bind(c.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    for (name, level) in [(SYSADMIN, 1), (DUAL, 5)] {
        app.exec(&format!(
            r#"INSERT INTO users (username, password, name, status)
               SELECT '{name}', 'x', '{name}', 1
               WHERE NOT EXISTS (SELECT 1 FROM users WHERE username = '{name}');
               INSERT INTO "userCompany" ("userId", "companyId", status, "idleTimeInSecond", "isDefault")
               SELECT u.id, {0}, 1, 300, true FROM users u WHERE u.username = '{name}'
                 AND NOT EXISTS (SELECT 1 FROM "userCompany" x WHERE x."userId" = u.id AND x."companyId" = {0});
               INSERT INTO "userAccessLevel" ("userId", "accessLevelId", "companyId", status)
               SELECT u.id, {level}, {0}, 1 FROM users u WHERE u.username = '{name}'
                 AND NOT EXISTS (SELECT 1 FROM "userAccessLevel" x WHERE x."userId" = u.id AND x."companyId" = {0});"#,
            c.id
        ))
        .await;
    }
    // DUAL: anggota company lain juga (bukan default); selalu pulihkan keadaan awal.
    app.exec(&format!(
        r#"INSERT INTO "userCompany" ("userId", "companyId", status, "idleTimeInSecond", "isDefault")
           SELECT u.id, {other_id}, 1, 300, false FROM users u WHERE u.username = '{DUAL}'
             AND NOT EXISTS (SELECT 1 FROM "userCompany" x WHERE x."userId" = u.id AND x."companyId" = {other_id});
           UPDATE "userCompany" SET status = 1, "isDefault" = ("companyId" = {0})
           WHERE "userId" = (SELECT id FROM users WHERE username = '{DUAL}');
           INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
           SELECT al.id, f.k, a::"PermissionAction" FROM "accessLevel" al
           CROSS JOIN (VALUES ('READ'), ('WRITE'), ('DELETE')) AS p(a)
           CROSS JOIN (VALUES ('company-settings'), ('access-levels')) AS f(k)
           WHERE al."isSystem" AND al.code IN ('SYSTEM_ADMIN', 'SUPER_ADMIN', 'ADMIN', 'MANAGER')
           ON CONFLICT DO NOTHING;"#,
        c.id
    ))
    .await;
    let uid = |n: &'static str| {
        let app = &app;
        async move {
            app.scalar::<i32>(&format!("SELECT id FROM users WHERE username = '{n}'"))
                .await
        }
    };
    let sysadmin = uid(SYSADMIN).await;
    let _ = uid(DUAL).await;
    Some(Ctx {
        app,
        c,
        channel,
        sysadmin,
        other_uuid,
    })
}

/// Tunggu baris `auditLog` (ditulis async setelah respons) sampai jumlahnya ≥ `min`.
async fn audit_count(app: &TestApp, filter: &str, min: i64) -> i64 {
    let mut n = 0;
    for _ in 0..60 {
        n = app
            .scalar(&format!(
                r#"SELECT count(*) FROM "auditLog" WHERE {filter}"#
            ))
            .await;
        if n >= min {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    n
}

/// Pendengar channel invalidasi (subscriber terpisah).
struct Listener {
    rx: tokio::sync::broadcast::Receiver<Message>,
    _client: SubscriberClient,
}

async fn listen(channel: &str) -> Listener {
    let client = Builder::default_centralized()
        .build_subscriber_client()
        .unwrap();
    client.init().await.unwrap();
    client.subscribe(channel).await.unwrap();
    let rx = client.message_rx();
    Listener {
        rx,
        _client: client,
    }
}

impl Listener {
    /// Ada pesan `{kind, key: id}` dalam 3 detik?
    async fn saw(&mut self, kind: &str, key: &str, id: i64) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while let Ok(Ok(msg)) = tokio::time::timeout_at(deadline, self.rx.recv()).await {
            let Some(raw) = msg.value.as_string() else {
                continue;
            };
            let v: Value = serde_json::from_str(&raw).unwrap_or_default();
            if v["kind"] == kind && v[key] == id {
                return true;
            }
        }
        false
    }
}

fn denied(res: &TestResponse) {
    // RolesGuard menjawab 401 `Access denied: insufficient permissions` (bukan 403).
    assert!(
        matches!(res.status.as_u16(), 401 | 403),
        "harus ditolak, dapat {} {}",
        res.status,
        res.text()
    );
}

#[tokio::test]
async fn it_companies_list_and_get() {
    let _g = SERIAL.lock().await;
    let Some(ctx) = setup().await else { return };
    let (app, c) = (&ctx.app, &ctx.c);

    // Tanpa token → 401; SUPER_ADMIN/STAFF tidak boleh daftar semua company.
    app.get("/companies").send().await.assert_status(401);
    denied(
        &app.get("/companies")
            .token(&c.super_admin.token)
            .send()
            .await,
    );
    denied(&app.get("/companies").token(&c.staff.token).send().await);

    // Bawaan: semua company aktif, urut createdAt desc, tanpa profilePictureFullPath.
    let res = app.get("/companies").token(&ctx.sys()).send().await;
    res.assert_status(200).assert_no_sql_error();
    let total: i64 = app
        .scalar("SELECT count(*) FROM company WHERE status = 1")
        .await;
    assert_eq!(res.json["success"], true);
    assert_eq!(res.json["message"], "Success get all data company");
    assert_eq!(res.json["data"]["total"], total);
    assert_eq!(res.json["data"]["itemsPerPage"], total);
    assert_eq!(res.json["data"]["pageNumber"], 1);
    let result = res.json["data"]["result"].as_array().unwrap();
    assert_eq!(result.len() as i64, total);
    let first = result[0].as_object().unwrap();
    assert!(!first.contains_key("profilePictureFullPath"));
    assert!(first.contains_key("inviteCode") && first.contains_key("maxOrderDiscountPercent"));
    // Urutan kunci = urutan kolom Prisma (bukan alfabet).
    let raw = res.text();
    assert!(
        raw.find("\"id\"") < raw.find("\"uuid\"") && raw.find("\"uuid\"") < raw.find("\"name\""),
        "urutan kunci: {}",
        &raw[..200.min(raw.len())]
    );
    let created: Vec<&str> = result
        .iter()
        .map(|r| r["createdAt"].as_str().unwrap())
        .collect();
    assert!(created.windows(2).all(|w| w[0] >= w[1]), "createdAt desc");

    // Pencarian (ILIKE nama/telp) + paginasi + urut.
    let res = app
        .get("/companies?pageNumber=1&itemsPerPage=2&orderBy=asc&orderColumn=name")
        .token(&ctx.sys())
        .send()
        .await;
    res.assert_status(200);
    let names: Vec<String> = res.json["data"]["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names.len(), 2);
    assert_eq!(res.json["data"]["itemsPerPage"], 2);
    assert_eq!(res.json["data"]["total"], total);
    let name: String = app
        .scalar(&format!("SELECT name FROM company WHERE id = {}", c.id))
        .await;
    let res = app
        .get(&format!(
            "/companies?search={}",
            name.to_lowercase().replace(' ', "%20")
        ))
        .token(&ctx.sys())
        .send()
        .await;
    res.assert_status(200);
    assert!(
        res.json["data"]["result"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == c.id)
    );

    // Validasi query (400, pesan array) dan kombinasi yang di Node menjadi galat Prisma (500).
    let res = app
        .get("/companies?pageNumber=abc&orderBy=1")
        .token(&ctx.sys())
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!(["pageNumber must be a number conforming to the specified constraints"])
    );
    app.get("/companies?pageNumber=1")
        .token(&ctx.sys())
        .send()
        .await
        .assert_status(500);
    app.get("/companies?pageNumber=1&itemsPerPage=2&orderColumn=nope&orderBy=asc")
        .token(&ctx.sys())
        .send()
        .await
        .assert_status(500);

    // GET /companies/:uuid
    let uri = format!("/companies/{}", c.uuid);
    let res = app.get(&uri).token(&c.super_admin.token).send().await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(res.json["success"], true);
    assert_eq!(
        res.json["message"],
        format!("Success get data company by uuid {}", c.uuid)
    );
    let data = &res.json["data"];
    assert_eq!(data["uuid"], c.uuid);
    assert!(data.get("profilePictureFullPath").is_none());
    let wp = &data["workPattern"];
    assert_eq!(wp["current"]["workDays"], 5);
    assert_eq!(wp["current"]["offDays"], json!([6, 7]));
    assert!(wp["current"]["effectiveFrom"].is_null());
    assert!(wp["scheduled"].is_null());
    assert_eq!(
        wp["normalHours"],
        json!({"workday": 8, "shortDay": null, "weekly": 40})
    );
    assert!(wp["nextEffectiveDate"].as_str().unwrap().len() == 10);
    // `workPattern` ada di akhir objek.
    let raw = res.text();
    assert!(raw.rfind("\"workPattern\"") > raw.rfind("\"updatedAt\""));

    // Peran tanpa izin, bukan anggota company, uuid rusak.
    denied(&app.get(&uri).token(&c.manager.token).send().await);
    denied(&app.get(&uri).token(&c.staff.token).send().await);
    let res = app
        .get(&format!("/companies/{}", ctx.other_uuid))
        .token(&c.super_admin.token)
        .send()
        .await;
    res.assert_status(401);
    assert_eq!(
        res.json["message"],
        "You do not have access to this company"
    );
    app.get("/companies/not-a-uuid")
        .token(&c.super_admin.token)
        .send()
        .await
        .assert_status(400);
    // `GET /companies/invite-link` tidak ada lagi (dihapus juga di Node): jatuh ke `GET :uuid` → 400.
    let res = app
        .get("/companies/invite-link")
        .token(&c.super_admin.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(res.json["message"], "Validation failed (uuid is expected)");
}

#[tokio::test]
async fn it_companies_patch() {
    let _g = SERIAL.lock().await;
    let Some(ctx) = setup().await else { return };
    let (app, c) = (&ctx.app, &ctx.c);
    let uri = format!("/companies/{}", c.uuid);
    let sa = &c.super_admin.token;
    let orig: Value = sqlx::query_scalar(
        r#"SELECT jsonb_build_object('name', name, 'telp', telp, 'address', address, 'tz', "timeZone",
             'week', "workweekStartDay", 'min', "storeVisitMinDurationMinutes",
             'max', "storeVisitMaxDurationHours", 'blur', "imageBlurRoles", 'cur', currency)
           FROM company WHERE id = $1"#,
    )
    .bind(c.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    let before_audit: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "auditLog" WHERE "entityType" = 'company' AND "entityId" = {} AND action = 'update'"#,
            c.id
        ))
        .await;
    let mut listen = listen(&ctx.channel).await;
    app.exec(&format!(
        r#"DELETE FROM "companyWorkPattern" WHERE "companyId" = {}"#,
        c.id
    ))
    .await;

    // Validasi: urutan pesan class-validator; tanpa token 401; peran tanpa izin ditolak.
    app.patch(&uri)
        .json(&json!({}))
        .send()
        .await
        .assert_status(401);
    let res = app.patch(&uri).token(sa).json(&json!({})).send().await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "name should not be empty",
            "name must be a string",
            "timeZone should not be empty",
            "timeZone must be a string"
        ])
    );
    let res = app
        .patch(&uri)
        .token(sa)
        .json(
            &json!({"name": "x", "timeZone": "UTC", "workweekStartDay": 9, "modules": [],
                       "workPattern": {"workDays": 7}}),
        )
        .send()
        .await;
    res.assert_status(400);
    let msgs = res.json["message"].as_array().unwrap();
    assert_eq!(msgs[0], "workweekStartDay must not be greater than 6");
    assert!(msgs.contains(&json!("modules should not be empty")));
    assert!(msgs.contains(&json!(
        "workPattern.workDays must be one of the following values: 5, 6"
    )));
    let body = json!({"name": "X", "timeZone": "UTC"});
    denied(
        &app.patch(&uri)
            .token(&c.manager.token)
            .json(&body)
            .send()
            .await,
    );
    denied(
        &app.patch(&uri)
            .token(&c.staff.token)
            .json(&body)
            .send()
            .await,
    );
    // Bukan anggota company itu → 401 pesan khusus (bukan dari RolesGuard).
    let res = app
        .patch(&format!("/companies/{}", ctx.other_uuid))
        .token(sa)
        .json(&body)
        .send()
        .await;
    res.assert_status(401);
    assert_eq!(
        res.json["message"],
        "You do not have access to this company"
    );

    // Jalur sederhana: kolom biasa, `name` di-trim, kunci di luar DTO diabaikan.
    let max_users: i32 = app
        .scalar(&format!(
            "SELECT \"maxUsers\" FROM company WHERE id = {}",
            c.id
        ))
        .await;
    let res = app
        .patch(&uri)
        .token(sa)
        .json(&json!({
            "name": "  IT Renamed  ", "timeZone": "Asia/Jakarta", "telp": "0812",
            "address": " Jl. Test ", "imageBlurRoles": ["ADMIN"], "maxOrderDiscountPercent": "12.5",
            "dailyReportSentAt": "2026-11-16T08:30:00.000Z", "maxUsers": 9999
        }))
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(res.json["success"], true);
    assert_eq!(res.json["message"], "update company success");
    let d = &res.json["data"];
    assert_eq!(d["name"], "IT Renamed");
    assert_eq!(d["address"], "Jl. Test");
    assert_eq!(d["timeZone"], "Asia/Jakarta");
    assert_eq!(d["imageBlurRoles"], json!(["ADMIN"]));
    assert_eq!(d["maxOrderDiscountPercent"], "12.5");
    assert_eq!(d["dailyReportSentAt"], "1970-01-01T08:30:00.000Z");
    assert!(d.get("profilePictureFullPath").is_none());
    assert_eq!(d["workPattern"]["current"]["workDays"], 5);
    assert_eq!(
        app.scalar::<i32>(&format!(
            "SELECT \"maxUsers\" FROM company WHERE id = {}",
            c.id
        ))
        .await,
        max_users,
        "kolom di luar DTO tidak boleh ikut ditulis"
    );
    let n = audit_count(
        app,
        &format!(
            r#""entityType" = 'company' AND "entityId" = {} AND action = 'update'"#,
            c.id
        ),
        before_audit + 1,
    )
    .await;
    assert!(n > before_audit, "update company harus ter-audit");

    // Pasangan durasi dinilai terhadap baris tersimpan; 422 tidak meninggalkan perubahan.
    let res = app
        .patch(&uri)
        .token(sa)
        .json(
            &json!({"name": "IT Renamed", "timeZone": "UTC", "storeVisitMinDurationMinutes": 600,
                       "storeVisitMaxDurationHours": 8}),
        )
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(res.json["code"], "STORE_VISIT_MIN_DURATION_TOO_LONG");
    assert_eq!(
        res.json["details"],
        json!({"minDurationMinutes": 600, "maxDurationHours": 8})
    );
    let tz: String = app
        .scalar(&format!(
            "SELECT \"timeZone\" FROM company WHERE id = {}",
            c.id
        ))
        .await;
    assert_eq!(tz, "Asia/Jakarta", "rollback: timeZone tidak boleh berubah");
    app.patch(&uri)
        .token(sa)
        .json(&json!({"name": "IT Renamed", "timeZone": "Asia/Jakarta",
                       "storeVisitMinDurationMinutes": 5, "storeVisitMaxDurationHours": 9}))
        .send()
        .await
        .assert_status(200);
    assert_eq!(
        app.scalar::<i32>(&format!(
            "SELECT \"storeVisitMaxDurationHours\" FROM company WHERE id = {}",
            c.id
        ))
        .await,
        9
    );

    // Pola kerja: jadwalkan, ganti, samakan dengan pola berlaku (tidak menulis), batalkan.
    let wp = |v: Value| json!({"name": "IT Renamed", "timeZone": "Asia/Jakarta", "workPattern": v});
    let res = app
        .patch(&uri)
        .token(sa)
        .json(&wp(json!({"workDays": 6, "offDays": [7], "shortDay": 6})))
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    let view = &res.json["data"]["workPattern"];
    assert_eq!(
        view["current"]["workDays"], 5,
        "pola baru berlaku minggu depan"
    );
    assert_eq!(view["scheduled"]["workDays"], 6);
    assert_eq!(view["scheduled"]["offDays"], json!([7]));
    assert_eq!(view["scheduled"]["shortDay"], 6);
    assert_eq!(
        view["scheduled"]["effectiveFrom"],
        view["nextEffectiveDate"]
    );
    let pattern_audit = audit_count(
        app,
        &format!(
            r#""entityType" = 'company' AND "entityId" = {} AND "subEntity" = 'workPattern'"#,
            c.id
        ),
        1,
    )
    .await;
    assert!(pattern_audit >= 1, "companyWorkPattern harus ter-audit");

    // Mengganti jadwal: baris lama di-supersede, baris baru satu-satunya yang aktif.
    app.patch(&uri)
        .token(sa)
        .json(&wp(json!({"workDays": 5, "offDays": [1, 7]})))
        .send()
        .await
        .assert_status(200);
    let live: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "companyWorkPattern" WHERE "companyId" = {} AND "supersededAt" IS NULL"#,
            c.id
        ))
        .await;
    let total: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "companyWorkPattern" WHERE "companyId" = {}"#,
            c.id
        ))
        .await;
    assert_eq!((live, total), (1, 2));

    // Realign: ganti workweekStartDay memindahkan tanggal berlaku jadwal.
    let before: String = app
        .scalar(&format!(
            r#"SELECT "effectiveFrom"::text FROM "companyWorkPattern"
               WHERE "companyId" = {} AND "supersededAt" IS NULL"#,
            c.id
        ))
        .await;
    let week: i32 = orig["week"].as_i64().unwrap() as i32;
    let res = app
        .patch(&uri)
        .token(sa)
        .json(&json!({"name": "IT Renamed", "timeZone": "Asia/Jakarta", "workweekStartDay": (week + 3) % 7}))
        .send()
        .await;
    res.assert_status(200);
    let after = res.json["data"]["workPattern"]["scheduled"]["effectiveFrom"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(before, after, "jadwal harus pindah ke awal minggu baru");
    assert_eq!(
        res.json["data"]["workPattern"]["nextEffectiveDate"]
            .as_str()
            .unwrap(),
        after
    );

    // Pola yang tak berbeda dengan yang berlaku (default 5 hari Sab-Min) tidak menulis baris.
    app.patch(&uri)
        .token(sa)
        .json(&json!({"name": "IT Renamed", "timeZone": "Asia/Jakarta", "cancelScheduledWorkPattern": true}))
        .send()
        .await
        .assert_status(200);
    let live: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "companyWorkPattern" WHERE "companyId" = {} AND "supersededAt" IS NULL"#,
            c.id
        ))
        .await;
    assert_eq!(live, 0, "cancel membatalkan jadwal");
    let res = app
        .patch(&uri)
        .token(sa)
        .json(&wp(json!({"workDays": 5, "offDays": [7, 6]})))
        .send()
        .await;
    res.assert_status(200);
    assert!(res.json["data"]["workPattern"]["scheduled"].is_null());

    // 422 WORK_PATTERN_INVALID: konflik dan aturan lintas-field; tidak ada yang tersimpan.
    let res = app
        .patch(&uri)
        .token(sa)
        .json(
            &json!({"name": "NOPE", "timeZone": "UTC", "cancelScheduledWorkPattern": true,
                       "workPattern": {"workDays": 5, "offDays": [6, 7]}}),
        )
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(res.json["code"], "WORK_PATTERN_INVALID");
    assert_eq!(res.json["details"], json!({"field": "conflict"}));
    let res = app
        .patch(&uri)
        .token(sa)
        .json(&json!({"name": "NOPE", "timeZone": "UTC", "workPattern": {"workDays": 5, "offDays": [7]}}))
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(res.json["details"], json!({"field": "offDays"}));
    let name: String = app
        .scalar(&format!("SELECT name FROM company WHERE id = {}", c.id))
        .await;
    assert_eq!(name, "IT Renamed");

    // Modul: REPLACE; invalidasi `company-modules` + cache `user-me-*` seluruh anggota.
    let me_key = format!("user-me-{}", c.staff.id);
    app.state
        .redis
        .next()
        .set::<(), _, _>(&me_key, "cached", None, None, false)
        .await
        .unwrap();
    let res = app
        .patch(&uri)
        .token(sa)
        .json(&json!({"name": "IT Renamed", "timeZone": "Asia/Jakarta", "modules": ["TIME_TRACKING"]}))
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["data"]["moduleTimeTrackingEnabled"], true);
    assert_eq!(res.json["data"]["moduleSalesTrackerEnabled"], false);
    let still: Option<String> = app.state.redis.next().get(&me_key).await.unwrap();
    assert!(
        still.is_none(),
        "user-me-<id> harus dibuang sebelum respons"
    );
    assert!(
        listen
            .saw("company-modules", "companyId", i64::from(c.id))
            .await,
        "pesan protokol company-modules harus terkirim"
    );
    app.patch(&uri)
        .token(sa)
        .json(&json!({"name": "IT Renamed", "timeZone": "Asia/Jakarta", "modules": ["TIME_TRACKING", "SALES_TRACKER"]}))
        .send()
        .await
        .assert_status(200);

    // Pulihkan fixture.
    app.exec(&format!(
        r#"DELETE FROM "companyWorkPattern" WHERE "companyId" = {id};
           UPDATE company SET name = $q${name}$q$, telp = {telp}, address = {address}, "timeZone" = $q${tz}$q$,
             "workweekStartDay" = {week}, "storeVisitMinDurationMinutes" = {min},
             "storeVisitMaxDurationHours" = {max}, "imageBlurRoles" = ARRAY[]::text[],
             "maxOrderDiscountPercent" = NULL,
             "moduleTimeTrackingEnabled" = true, "moduleSalesTrackerEnabled" = true WHERE id = {id};"#,
        id = c.id,
        name = orig["name"].as_str().unwrap(),
        telp = orig["telp"].as_str().map_or("NULL".to_owned(), |s| format!("$q${s}$q$")),
        address = orig["address"].as_str().map_or("NULL".to_owned(), |s| format!("$q${s}$q$")),
        tz = orig["tz"].as_str().unwrap(),
        week = orig["week"],
        min = orig["min"],
        max = orig["max"],
    ))
    .await;
}

#[tokio::test]
async fn it_companies_delete_invite_and_presign() {
    let _g = SERIAL.lock().await;
    let Some(ctx) = setup().await else { return };
    let (app, c) = (&ctx.app, &ctx.c);

    // DELETE: soft delete, hanya SYSTEM_ADMIN, ter-audit.
    let (tid, tuuid): (i32, String) = sqlx::query_as(
        "INSERT INTO company (name, \"timeZone\", status) VALUES ('IT Throwaway', 'UTC', 1) RETURNING id, uuid::text",
    )
    .fetch_one(app.db())
    .await
    .unwrap();
    let uri = format!("/companies/{tuuid}");
    denied(&app.delete(&uri).token(&c.super_admin.token).send().await);
    denied(&app.delete(&uri).token(&c.admin.token).send().await);
    app.delete(&uri).send().await.assert_status(401);
    let res = app.delete(&uri).token(&ctx.sys()).send().await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(
        res.json,
        json!({"success": true, "message": "delete company success", "data": {"isDeleted": true}})
    );
    assert_eq!(
        app.scalar::<i32>(&format!("SELECT status FROM company WHERE id = {tid}"))
            .await,
        0
    );
    let n = audit_count(
        app,
        // Transisi `status` 1 → 0 dicatat sebagai `delete` (soft-delete signature).
        &format!(r#""entityType" = 'company' AND "entityId" = {tid} AND action = 'delete'"#),
        1,
    )
    .await;
    assert_eq!(n, 1);
    // uuid tak dikenal → Prisma P2025 → 500; uuid rusak → 400.
    let res = app
        .delete(&format!("/companies/{}", uuid::Uuid::new_v4()))
        .token(&ctx.sys())
        .send()
        .await;
    res.assert_status(500);
    app.delete("/companies/xyz")
        .token(&ctx.sys())
        .send()
        .await
        .assert_status(400);
    // Company yang sudah dihapus tidak muncul lagi di GET (milik sysadmin: bukan anggota → 401).
    app.exec(&format!("DELETE FROM company WHERE id = {tid}"))
        .await;

    // Kode undangan.
    let orig: Option<String> =
        sqlx::query_scalar("SELECT \"inviteCode\" FROM company WHERE id = $1")
            .bind(c.id)
            .fetch_one(app.db())
            .await
            .unwrap();
    app.exec(&format!(
        "UPDATE company SET \"inviteCode\" = NULL WHERE id = {}",
        c.id
    ))
    .await;
    app.post("/companies/invite/regenerate")
        .send()
        .await
        .assert_status(401);
    denied(
        &app.post("/companies/invite/regenerate")
            .token(&c.manager.token)
            .send()
            .await,
    );
    denied(
        &app.post("/companies/invite/regenerate")
            .token(&c.staff.token)
            .send()
            .await,
    );
    let res = app
        .post("/companies/invite/regenerate")
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    let code = res.json["inviteCode"].as_str().unwrap().to_owned();
    assert_eq!(code.len(), 10);
    assert!(
        code.chars()
            .all(|ch| "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz23456789".contains(ch))
    );
    assert_eq!(res.json["inviteLink"], format!("{FE}/join/{code}"));
    let raw = res.text();
    assert!(raw.find("inviteCode") < raw.find("inviteLink"));
    let stored: Option<String> = app
        .scalar(&format!(
            "SELECT \"inviteCode\" FROM company WHERE id = {}",
            c.id
        ))
        .await;
    assert_eq!(stored.as_deref(), Some(code.as_str()));
    // Panggilan kedua mengembalikan kode yang sama (Node tidak benar-benar me-regenerate).
    let res = app
        .post("/companies/invite/regenerate")
        .token(&c.super_admin.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["inviteCode"], code);
    // `inviteCode` dibuang kebijakan kolom audit (`policy.rs`), jadi tidak ada baris audit — sama Node.

    match orig {
        Some(o) => {
            app.exec(&format!(
                "UPDATE company SET \"inviteCode\" = '{o}' WHERE id = {}",
                c.id
            ))
            .await;
        }
        None => {
            app.exec(&format!(
                "UPDATE company SET \"inviteCode\" = NULL WHERE id = {}",
                c.id
            ))
            .await;
        }
    }

    // Presigned URL logo (hanya SYSTEM_ADMIN; kunci layanan uji, tanpa jaringan).
    let base = format!("/companies/{}/profile-picture/presigned-url", c.uuid);
    let res = app
        .get(&format!(
            "{base}?fileName=logo.png&fileExtension=png&fileSize=900"
        ))
        .token(&ctx.sys())
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["success"], true);
    assert_eq!(res.json["message"], "success get presigned url");
    let url = res.json["url"].as_str().unwrap();
    assert!(
        url.starts_with("https://storage.googleapis.com/test-bucket/"),
        "{url}"
    );
    assert!(
        url.contains("logo.png") && url.contains("X-Goog-Signature"),
        "{url}"
    );
    denied(
        &app.get(&format!(
            "{base}?fileName=logo.png&fileExtension=png&fileSize=900"
        ))
        .token(&c.super_admin.token)
        .send()
        .await,
    );
    let res = app
        .get(&format!(
            "{base}?fileName=logo.gif&fileExtension=gif&fileSize=2000000"
        ))
        .token(&ctx.sys())
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "Invalid file type. Only JPG, JPEG, and PNG files are allowed.",
            "File size must be less than 1000000."
        ])
    );
    let res = app.get(&base).token(&ctx.sys()).send().await;
    res.assert_status(400);
    assert_eq!(res.json["message"].as_array().unwrap().len(), 7);
    app.get(
        "/companies/nope/profile-picture/presigned-url?fileName=a&fileExtension=png&fileSize=1",
    )
    .token(&ctx.sys())
    .send()
    .await
    .assert_status(400);
}

/// Celah lintas-tenant (diperbaiki 2026-11-17): admin company LAIN tidak boleh mengubah company
/// fixture (`PATCH` -> 401 `userBelongsToCompany`) (route `switch` tidak ada lagi di Rust).
#[tokio::test]
async fn it_companies_cross_tenant() {
    let _g = SERIAL.lock().await;
    let Some(ctx) = setup().await else { return };
    let (app, c) = (&ctx.app, &ctx.c);
    let other = app.other_company_admin().await;

    let before: String = app
        .scalar(&format!("SELECT name FROM company WHERE id = {}", c.id))
        .await;
    let res = app
        .patch(&format!("/companies/{}", c.uuid))
        .token(&other.admin.token)
        .json(&json!({"name": "Hijacked", "timeZone": "UTC"}))
        .send()
        .await;
    res.assert_status(401);
    assert_eq!(
        res.json["message"],
        "You do not have access to this company"
    );
    assert_eq!(
        app.scalar::<String>(&format!("SELECT name FROM company WHERE id = {}", c.id))
            .await,
        before
    );
    assert_eq!(
        app.scalar::<i64>(&format!(
            r#"SELECT COUNT(*) FROM "userCompany" WHERE "userId" = {} AND "companyId" = {}"#,
            other.admin.id, c.id
        ))
        .await,
        0
    );
}
