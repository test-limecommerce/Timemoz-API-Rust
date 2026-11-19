//! Uji integrasi route settings (4) + status (1) lembur lewat HTTP (router dengan gerbang
//! lengkap + layer audit) terhadap Postgres & Redis nyata.
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-overtime --test it_ot_settings`.
//! Nama binary unik (database test diberi nama dari nama binary).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::Duration;

use axum::Router;
use mz_core::AppState;
use mz_testkit::{Company, TestApp};
use serde_json::{Value, json};
use tokio::sync::Mutex;

/// Semua test memakai company fixture yang sama: dijalankan berurutan.
static SERIAL: Mutex<()> = Mutex::const_new(());

const LEGAL_NOTE: &str = mz_overtime::hr::common::constants::LEGAL_NOTE;

fn build(state: &AppState) -> Router {
    mz_overtime::hr::settings::router(state)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

/// Aplikasi + company dengan data lembur kosong (zona Asia/Jakarta).
async fn setup() -> Option<(TestApp, Company)> {
    let app = TestApp::new(build).await?;
    let c = app.company().await;
    // Template anonim belum memberi role bawaan izin fitur lembur (produksi: backfill
    // registry Node; `overtime-report`/`overtime-settings` `withheldFromBuiltIn: STAFF`).
    app.exec(
        r#"INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
           SELECT al.id, f.k, f.a::"PermissionAction" FROM "accessLevel" al
           CROSS JOIN (VALUES ('overtime', 'READ'), ('overtime', 'WRITE'), ('overtime-report', 'READ'),
                              ('overtime-settings', 'READ'), ('overtime-settings', 'WRITE')) AS f(k, a)
           WHERE al."isSystem" AND (al.code IN ('SYSTEM_ADMIN', 'SUPER_ADMIN', 'ADMIN', 'MANAGER')
                                    OR (al.code = 'STAFF' AND f.k = 'overtime'))
           ON CONFLICT DO NOTHING"#,
    )
    .await;
    app.exec(&format!(
        r#"DELETE FROM "overtimeRequest" WHERE "companyId" = {0};
           DELETE FROM "overtimeApprover" WHERE "companyId" = {0};
           DELETE FROM "companyOvertimeSetting" WHERE "companyId" = {0};
           UPDATE company SET "timeZone" = 'Asia/Jakarta' WHERE id = {0};"#,
        c.id
    ))
    .await;
    Some((app, c))
}

/// Satu `overtimeRequest` mentah.
async fn request(
    app: &TestApp,
    c: &Company,
    user: i32,
    kind: &str,
    status: &str,
    date: &str,
    deadline: &str,
) -> i32 {
    app.scalar(&format!(
        r#"WITH ins AS (
             INSERT INTO "overtimeRequest" ("companyId", "userId", "requestedById", kind, date, "requestedSec",
               reason, status, "retroWindowDays", "responseDeadline", "finalAt")
             VALUES ({c}, {user}, {user}, '{kind}', '{date}', 3600, 'uji', '{status}', 7, '{deadline}', '{deadline}')
             RETURNING id)
           SELECT id FROM ins"#,
        c = c.id
    ))
    .await
}

/// Tanggal lokal Jakarta + `days`.
async fn jkt_date(app: &TestApp, days: i32) -> String {
    app.scalar(&format!(
        "SELECT ((now() AT TIME ZONE 'Asia/Jakarta')::date + {days})::text"
    ))
    .await
}

async fn audit_count(app: &TestApp, c: &Company, sub: &str, actor: i32) -> i64 {
    // Flush audit berjalan di latar sesudah respons.
    let mut n = 0;
    for _ in 0..40 {
        n = app
            .scalar(&format!(
                r#"SELECT count(*) FROM "auditLog" WHERE "companyId" = {} AND "subEntity" = '{sub}'
                     AND "actorId" = {actor}"#,
                c.id
            ))
            .await;
        if n > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    n
}

fn uuids(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x["userId"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn gates_auth_and_roles() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let routes = [
        ("GET", "/v1/overtime/settings"),
        ("GET", "/v1/overtime/settings/approver-candidates"),
        ("GET", "/v1/overtime/settings/disable-impact"),
        ("PUT", "/v1/overtime/settings"),
        ("GET", "/v1/overtime/status"),
    ];
    for (m, p) in routes {
        let req = if m == "GET" {
            app.get(p)
        } else {
            app.put(p).json(&json!({}))
        };
        req.send().await.assert_status(401);
    }
    // MANAGER & STAFF sengaja tidak ada di @Roles settings.
    for user in [&c.manager, &c.staff] {
        for (m, p) in &routes[..4] {
            let req = if *m == "GET" {
                app.get(p)
            } else {
                app.put(p).json(&json!({}))
            };
            let res = req.token(&user.token).send().await;
            res.assert_status(401);
            assert_eq!(
                res.json,
                json!({"message": "Access denied: insufficient permissions", "error": "Unauthorized", "statusCode": 401})
            );
        }
    }
    // Status: STAFF boleh.
    app.get("/v1/overtime/status")
        .token(&c.staff.token)
        .send()
        .await
        .assert_status(200);
}

#[tokio::test]
async fn get_settings_seeds_defaults_and_owner() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let res = app
        .get("/v1/overtime/settings")
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(200);
    let d = res.data();
    assert_eq!(d["enabled"], true);
    assert_eq!(d["retroWindowDays"], 7);
    assert_eq!(d["legalNote"], LEGAL_NOTE);
    let approvers = d["approvers"].as_array().cloned().unwrap_or_default();
    assert_eq!(d["reminders"]["noApprovers"], approvers.is_empty());
    assert_wire_order(
        &res.text(),
        "\"data\"",
        &[
            "enabled",
            "retroWindowDays",
            "approvers",
            "reminders",
            "legalNote",
        ],
    );
    // Owner yang di-seed = SUPER_ADMIN (atau ownerUserId) yang efektif.
    assert_eq!(approvers.len(), 1, "owner SUPER_ADMIN tidak di-seed");
    if let Some(a) = approvers.first() {
        assert_eq!(a["role"], "SUPER_ADMIN");
        assert_wire_order(
            &res.text(),
            "\"approvers\"",
            &["userId", "name", "email", "role"],
        );
    }
    let rows: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "companyOvertimeSetting" WHERE "companyId" = {}"#,
            c.id
        ))
        .await;
    assert_eq!(rows, 1);
    // Panggilan kedua: jalur cepat, tanpa seed ulang.
    let again = app
        .get("/v1/overtime/settings")
        .token(&c.super_admin.token)
        .send()
        .await;
    again.assert_status(200);
    assert_eq!(again.data(), d);
}

#[tokio::test]
async fn approver_candidates_and_query_validation() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let res = app
        .get("/v1/overtime/settings/approver-candidates")
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(200);
    let ids = uuids(res.data());
    for u in [&c.super_admin, &c.admin, &c.manager] {
        assert!(ids.contains(&u.uuid), "kandidat {} hilang", u.uuid);
    }
    assert!(!ids.contains(&c.staff.uuid));
    // Urut nama.
    let names: Vec<String> = res
        .data()
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names.len(), sorted.len());

    // search email, tak peka huruf, di-trim.
    let term = c.admin.username.to_uppercase();
    let res = app
        .get(&format!(
            "/v1/overtime/settings/approver-candidates?search=%20{}%20",
            urlencode(&term)
        ))
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(200);
    assert!(uuids(res.data()).contains(&c.admin.uuid));

    let res = app
        .get(&format!(
            "/v1/overtime/settings/approver-candidates?search={}",
            "a".repeat(101)
        ))
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json,
        json!({"message": ["search must be shorter than or equal to 100 characters"], "error": "Bad Request", "statusCode": 400})
    );
    let res = app
        .get("/v1/overtime/settings/approver-candidates?search=a&search=b")
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "search must be shorter than or equal to 100 characters",
            "search must be a string"
        ])
    );
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::test]
async fn put_validation_400() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let put = |body: Value| {
        app.put("/v1/overtime/settings")
            .token(&c.admin.token)
            .json(&body)
            .send()
    };
    let res = put(json!({"retroWindowDays": "7"})).await;
    res.assert_status(400);
    assert_eq!(
        res.json,
        json!({
            "message": [
                "retroWindowDays must not be greater than 31",
                "retroWindowDays must not be less than 0",
                "retroWindowDays must be an integer number"
            ],
            "error": "Bad Request",
            "statusCode": 400
        })
    );
    let res = put(json!({"enabled": null, "approverUserIds": ["abc", "abc"]})).await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "enabled must be a boolean value",
            "each value in approverUserIds must be a UUID",
            "All approverUserIds's elements must be unique"
        ])
    );
    for bad in [
        json!({"expectedCancelCount": 1.5}),
        json!({"expectedCancelCount": 1_000_001}),
        json!({"retroWindowDays": 32}),
        json!({"approverUserIds": ["6ba7b810-9dad-11d1-80b4-00c04fd430c8"]}),
    ] {
        put(bad).await.assert_status(400);
    }
    // Validasi gagal → tidak ada yang dibuat.
    let rows: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "companyOvertimeSetting" WHERE "companyId" = {}"#,
            c.id
        ))
        .await;
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn put_replaces_approvers_and_retro_with_audit() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let res = app
        .put("/v1/overtime/settings")
        .token(&c.admin.token)
        .json(&json!({
            "retroWindowDays": 5,
            "approverUserIds": [c.manager.uuid, c.admin.uuid.to_uppercase(), c.manager.uuid]
        }))
        .send()
        .await;
    // ArrayUnique membandingkan string persis: duplikat → 400.
    res.assert_status(400);

    let res = app
        .put("/v1/overtime/settings")
        .token(&c.admin.token)
        .json(&json!({
            "retroWindowDays": 5,
            "approverUserIds": [c.manager.uuid, c.admin.uuid]
        }))
        .send()
        .await;
    res.assert_status(200);
    assert!(res.json.get("cancelled").is_none());
    let d = res.data();
    assert_eq!(d["retroWindowDays"], 5);
    let mut got = uuids(&d["approvers"]);
    got.sort();
    let mut want = vec![c.manager.uuid.clone(), c.admin.uuid.clone()];
    want.sort();
    assert_eq!(got, want);
    assert_eq!(d["reminders"]["noApprovers"], false);

    let (updated_by, retro): (Option<i32>, i32) = sqlx::query_as(
        r#"SELECT "updatedById", "retroWindowDays" FROM "companyOvertimeSetting" WHERE "companyId" = $1"#,
    )
    .bind(c.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!((updated_by, retro), (Some(c.admin.id), 5));
    let created_by: Vec<Option<i32>> = sqlx::query_scalar(
        r#"SELECT "createdById" FROM "overtimeApprover" WHERE "companyId" = $1 ORDER BY id"#,
    )
    .bind(c.id)
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(created_by, vec![Some(c.admin.id), Some(c.admin.id)]);

    assert!(audit_count(&app, &c, "overtimeSetting", c.admin.id).await >= 1);
    assert!(audit_count(&app, &c, "overtimeApprover", c.admin.id).await >= 1);

    // Himpunan kosong diizinkan → reminders.noApprovers.
    let res = app
        .put("/v1/overtime/settings")
        .token(&c.admin.token)
        .json(&json!({ "approverUserIds": [] }))
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.data()["approvers"], json!([]));
    assert_eq!(res.data()["reminders"]["noApprovers"], true);
}

#[tokio::test]
async fn put_rejects_ineligible_approver_422() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let ghost = "00000000-0000-4000-8000-000000000001";
    let res = app
        .put("/v1/overtime/settings")
        .token(&c.admin.token)
        .json(&json!({ "retroWindowDays": 3, "approverUserIds": [c.admin.uuid, c.staff.uuid, ghost] }))
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(
        res.json,
        json!({
            "code": "OVERTIME_APPROVER_NOT_ELIGIBLE",
            "message": "Approver harus anggota aktif dengan role Super Admin, Admin, atau Manager.",
            "details": { "userIds": [c.staff.uuid, ghost] }
        })
    );
    // Tidak ada yang commit.
    let retro: i32 = app
        .scalar(&format!(
            r#"SELECT "retroWindowDays" FROM "companyOvertimeSetting" WHERE "companyId" = {}"#,
            c.id
        ))
        .await;
    assert_eq!(retro, 7);
}

#[tokio::test]
async fn disable_flow_counts_confirms_and_cancels() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let today = jkt_date(&app, 0).await;
    let future = jkt_date(&app, 3).await;
    let past = jkt_date(&app, -3).await;
    let far = "2099-01-01T00:00:00Z";
    let pending = request(
        &app,
        &c,
        c.staff.id,
        "SELF",
        "PENDING_APPROVAL",
        &future,
        far,
    )
    .await;
    let approved_today = request(&app, &c, c.staff.id, "SELF", "APPROVED", &today, far).await;
    let approved_past = request(&app, &c, c.staff.id, "SELF", "APPROVED", &past, far).await;
    let completed = request(&app, &c, c.staff2.id, "SELF", "COMPLETED", &future, far).await;

    let res = app
        .get("/v1/overtime/settings/disable-impact")
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(
        res.json,
        json!({"data": {"pending": 1, "approved": 1, "total": 2}})
    );

    let put = |body: Value| {
        app.put("/v1/overtime/settings")
            .token(&c.admin.token)
            .json(&body)
            .send()
    };
    let res = put(json!({"enabled": false})).await;
    res.assert_status(400);
    assert_eq!(
        res.json,
        json!({
            "code": "OVERTIME_EXPECTED_CANCEL_COUNT_REQUIRED",
            "message": "Konfirmasi jumlah lembur yang akan dibatalkan (expectedCancelCount) wajib dikirim saat menonaktifkan fitur lembur."
        })
    );
    let res = put(json!({"enabled": false, "expectedCancelCount": 5, "retroWindowDays": 9})).await;
    res.assert_status(409);
    assert_eq!(
        res.json,
        json!({
            "code": "OVERTIME_DISABLE_IMPACT_CHANGED",
            "message": "Jumlah lembur yang akan dibatalkan sudah berubah. Periksa ulang lalu simpan lagi.",
            "details": {"pending": 1, "approved": 1, "total": 2}
        })
    );
    let state: (bool, i32) = sqlx::query_as(
        r#"SELECT "enabled", "retroWindowDays" FROM "companyOvertimeSetting" WHERE "companyId" = $1"#,
    )
    .bind(c.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(state, (true, 7));

    let res = put(json!({"enabled": false, "expectedCancelCount": 2})).await;
    res.assert_status(200);
    assert_eq!(res.json["cancelled"], 2);
    assert_eq!(res.data()["enabled"], false);
    let raw = res.text();
    assert!(
        raw.starts_with("{\"data\":") && raw.ends_with(",\"cancelled\":2}"),
        "{raw}"
    );

    let db = app.db().clone();
    let status_of = |id: i32| {
        let db = db.clone();
        async move {
            sqlx::query_as::<_, (String, Option<String>, bool)>(
            r#"SELECT status::text, "cancelReason"::text, "cancelledById" IS NULL FROM "overtimeRequest" WHERE id = $1"#,
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap()
        }
    };
    for id in [pending, approved_today] {
        assert_eq!(
            status_of(id).await,
            ("CANCELLED".into(), Some("FEATURE_DISABLED".into()), true)
        );
    }
    assert_eq!(status_of(approved_past).await.0, "APPROVED");
    assert_eq!(status_of(completed).await.0, "COMPLETED");
    assert!(audit_count(&app, &c, "overtimeSetting", c.admin.id).await >= 1);

    // Sudah nonaktif → no-op tanpa expectedCancelCount, tanpa `cancelled`.
    let res = put(json!({"enabled": false})).await;
    res.assert_status(200);
    assert!(res.json.get("cancelled").is_none());

    // false → true hanya membalik flag; yang dibatalkan tetap dibatalkan (FR-037).
    let res = put(json!({"enabled": true})).await;
    res.assert_status(200);
    assert_eq!(res.data()["enabled"], true);
    assert_eq!(status_of(pending).await.0, "CANCELLED");
}

#[tokio::test]
async fn status_counts_and_flags() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    // Seed default dulu, lalu approver = admin saja (approver tunggal).
    app.get("/v1/overtime/settings")
        .token(&c.admin.token)
        .send()
        .await
        .assert_status(200);
    app.exec(&format!(
        r#"DELETE FROM "overtimeApprover" WHERE "companyId" = {0};
           INSERT INTO "overtimeApprover" ("companyId", "userId") VALUES ({0}, {1});
           UPDATE "companyOvertimeSetting" SET "retroWindowDays" = 4 WHERE "companyId" = {0};"#,
        c.id, c.admin.id
    ))
    .await;
    let future = jkt_date(&app, 2).await;
    let future2 = jkt_date(&app, 4).await;
    let far = "2099-01-01T00:00:00Z";
    let gone = "2000-01-01T00:00:00Z";
    request(
        &app,
        &c,
        c.staff.id,
        "SELF",
        "PENDING_APPROVAL",
        &future,
        far,
    )
    .await;
    request(
        &app,
        &c,
        c.staff2.id,
        "SELF",
        "PENDING_APPROVAL",
        &future,
        gone,
    )
    .await;
    // Pengajuan admin sendiri: dihitung karena approver tunggal.
    request(
        &app,
        &c,
        c.admin.id,
        "SELF",
        "PENDING_APPROVAL",
        &future,
        far,
    )
    .await;
    request(
        &app,
        &c,
        c.staff.id,
        "ORDER",
        "PENDING_EMPLOYEE",
        &future2,
        far,
    )
    .await;

    let res = app
        .get("/v1/overtime/status")
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(200);
    let d = res.data();
    assert_wire_order(
        &res.text(),
        "\"data\"",
        &[
            "enabled",
            "isApprover",
            "canViewReport",
            "canManageSettings",
            "approverCount",
            "pendingApprovalCount",
            "pendingOrderCount",
            "legalNote",
            "timeZone",
            "retroWindowDays",
            "weeklyOffDays",
        ],
    );
    assert_eq!(d["enabled"], true);
    assert_eq!(d["isApprover"], true);
    assert_eq!(d["approverCount"], 1);
    assert_eq!(d["pendingApprovalCount"], 2);
    assert_eq!(d["pendingOrderCount"], 0);
    assert_eq!(d["legalNote"], LEGAL_NOTE);
    assert_eq!(d["timeZone"], "Asia/Jakarta");
    assert_eq!(d["retroWindowDays"], 4);
    assert!(
        d["weeklyOffDays"]
            .as_array()
            .is_some_and(|a| a.iter().all(Value::is_i64))
    );
    // ADMIN bawaan dengan izin tercentang: kedua gerbang lolos.
    assert_eq!(d["canViewReport"], true);
    assert_eq!(d["canManageSettings"], true);

    let res = app
        .get("/v1/overtime/status")
        .token(&c.staff.token)
        .send()
        .await;
    res.assert_status(200);
    let d = res.data();
    assert_eq!(d["isApprover"], false);
    assert_eq!(d["pendingApprovalCount"], 0);
    assert_eq!(d["pendingOrderCount"], 1);
    assert_eq!(d["canViewReport"], false);
    assert_eq!(d["canManageSettings"], false);

    // Approver kedua → pengajuan sendiri tak lagi dihitung.
    app.exec(&format!(
        r#"INSERT INTO "overtimeApprover" ("companyId", "userId") VALUES ({}, {})"#,
        c.id, c.manager.id
    ))
    .await;
    let res = app
        .get("/v1/overtime/status")
        .token(&c.admin.token)
        .send()
        .await;
    assert_eq!(res.data()["approverCount"], 2);
    assert_eq!(res.data()["pendingApprovalCount"], 1);

    // Zona tak dikenal → UTC.
    app.exec(&format!(
        r#"UPDATE company SET "timeZone" = 'Bogus/Zone' WHERE id = {}"#,
        c.id
    ))
    .await;
    let res = app
        .get("/v1/overtime/status")
        .token(&c.admin.token)
        .send()
        .await;
    assert_eq!(res.data()["timeZone"], "UTC");
}

/// Kunci tingkat atas objek pertama di teks JSON mentah `raw` sesudah `marker`, urut kemunculan.
fn wire_keys(raw: &str, marker: &str, keys: &[&str]) -> Vec<usize> {
    let start = raw.find(marker).unwrap_or(0);
    keys.iter()
        .map(|k| {
            raw[start..]
                .find(&format!("\"{k}\":"))
                .map_or(usize::MAX, |i| i + start)
        })
        .collect()
}

fn assert_wire_order(raw: &str, marker: &str, keys: &[&str]) {
    let pos = wire_keys(raw, marker, keys);
    let mut sorted = pos.clone();
    sorted.sort_unstable();
    assert!(
        pos.iter().all(|p| *p != usize::MAX) && pos == sorted,
        "urutan kunci {keys:?} di {raw}"
    );
}

#[tokio::test]
async fn employee_and_settings_locks_share_node_keys() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let db = app.db().clone();
    let mut tx = db.begin().await.unwrap();
    mz_overtime::locks::lock_employee(&mut tx, c.id, c.staff.id)
        .await
        .unwrap();
    // Kunci teks Node (`overtime-locks.ts`) dari koneksi lain: sudah dipegang.
    let mut other = db.begin().await.unwrap();
    let free: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtext('ot-emp:' || $1::text || ':' || $2::text))",
    )
    .bind(c.id)
    .bind(c.staff.id)
    .fetch_one(&mut *other)
    .await
    .unwrap();
    assert!(!free);
    other.rollback().await.unwrap();
    tx.rollback().await.unwrap();

    // Tanpa baris → None; sesudah ensure → nilai bawaan.
    let mut tx = db.begin().await.unwrap();
    assert!(
        mz_overtime::locks::lock_settings_for_share(&mut tx, c.id)
            .await
            .unwrap()
            .is_none()
    );
    tx.rollback().await.unwrap();
    mz_overtime::hr::common::settings_svc::ensure_settings(&db, c.id)
        .await
        .unwrap();
    let mut tx = db.begin().await.unwrap();
    let row = mz_overtime::locks::lock_settings_for_update(&mut tx, c.id)
        .await
        .unwrap()
        .unwrap();
    assert!(row.enabled);
    assert_eq!(row.retro_window_days, 7);
    tx.rollback().await.unwrap();
}

/// SQL fondasi `hr::common` yang dipakai modul lain (requests/approvals/reports) dijalankan
/// sungguhan: mapper, kelayakan, kuota, jam company.
#[tokio::test]
async fn common_sql_runs_against_real_schema() {
    use mz_overtime::hr::common::{
        clock, eligibility, mapper, quota,
        settings_svc::{effective_approvers, ensure_settings, is_effective_approver},
    };
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let db = app.db().clone();
    let date = jkt_date(&app, 1).await;
    let id = request(
        &app,
        &c,
        c.staff.id,
        "SELF",
        "PENDING_APPROVAL",
        &date,
        "2099-01-01T00:00:00Z",
    )
    .await;

    let sql = format!(
        "SELECT {} FROM {} WHERE r.id = $1",
        mapper::OVERTIME_REQUEST_COLUMNS,
        mapper::OVERTIME_REQUEST_FROM
    );
    let row: mapper::OvertimeRequestRow =
        sqlx::query_as(&sql).bind(id).fetch_one(&db).await.unwrap();
    assert_eq!(row.user_uuid, c.staff.uuid);
    let entity = mapper::to_overtime_request_entity(&row, None);
    assert_eq!(entity.date, date);
    assert_eq!(entity.duration_minutes, 60);
    assert!(entity.decided_by.is_none());

    let emps = eligibility::load_overtime_employees(&db, c.id, &[c.staff.id, c.admin.id, -1])
        .await
        .unwrap();
    assert_eq!(emps.len(), 2);
    assert!(
        emps.iter()
            .all(|e| eligibility::is_eligible_overtime_employee(Some(e)))
    );

    let mut conn = db.acquire().await.unwrap();
    let now = mz_core::time::now_naive();
    let clock = clock::company_clock(&mut conn, c.id, now).await.unwrap();
    assert_eq!(clock.time_zone, "Asia/Jakarta");
    let off = clock::current_off_days(&mut conn, c.id, Some("Asia/Jakarta"), now)
        .await
        .unwrap();
    assert!(!off.is_empty());

    let d = mz_overtime::zone::parse_date_key(&date).unwrap();
    let ev = quota::evaluate(
        &mut conn,
        &quota::QuotaEvaluateInput {
            company_id: c.id,
            user_id: c.staff.id,
            date: d,
            requested_sec: 3600,
            as_of_ms: now.and_utc().timestamp_millis(),
            workweek_start_day: clock.workweek_start_day,
            exclude_request_id: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(ev.view.requested_minutes, 60);
    let many = quota::evaluate_many(
        &mut conn,
        c.id,
        clock.workweek_start_day,
        now.and_utc().timestamp_millis(),
        &[
            quota::QuotaEvaluateManyItem {
                key: 1,
                user_id: c.staff.id,
                date: d,
                requested_sec: 3600,
                exclude_request_id: Some(id),
            },
            quota::QuotaEvaluateManyItem {
                key: 2,
                user_id: c.staff2.id,
                date: d + chrono::Duration::days(8),
                requested_sec: 7200,
                exclude_request_id: None,
            },
        ],
    )
    .await
    .unwrap();
    assert_eq!(many.len(), 2);
    drop(conn);

    ensure_settings(&db, c.id).await.unwrap();
    let approvers = effective_approvers(&db, c.id).await.unwrap();
    for a in &approvers {
        assert!(is_effective_approver(&db, c.id, a.id).await.unwrap());
    }
    assert!(!is_effective_approver(&db, c.id, c.staff.id).await.unwrap());
}
