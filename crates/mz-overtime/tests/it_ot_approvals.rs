//! Uji integrasi route approvals lembur (7) lewat HTTP (router dengan gerbang lengkap)
//! terhadap Postgres & Redis nyata.
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-overtime --test it_ot_approvals`.
//! Nama binary unik (database test diberi nama dari nama binary).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use mz_core::AppState;
use mz_testkit::{Company, TestApp};
use serde_json::{Value, json};
use tokio::sync::Mutex;

/// Semua test memakai company fixture yang sama: dijalankan berurutan.
static SERIAL: Mutex<()> = Mutex::const_new(());

const PIPE_400: &str = "Validation failed (uuid v 4 is expected)";
const RANDOM_V4: &str = "3f2b8c1e-5d44-4b0a-9a0c-1a2b3c4d5e6f";

fn build(state: &AppState) -> Router {
    mz_overtime::hr::approvals::router(state)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

/// Aplikasi + company dengan data lembur kosong (zona Asia/Jakarta), approver efektif =
/// ADMIN + MANAGER (≥ 2), fitur aktif, tanpa cuti disetujui yang mengganggu.
async fn setup() -> Option<(TestApp, Company)> {
    let app = TestApp::new(build).await?;
    let c = app.company().await;
    // Template anonim belum memberi role bawaan izin fitur lembur.
    app.exec(
        r#"INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
           SELECT al.id, f.k, f.a::"PermissionAction" FROM "accessLevel" al
           CROSS JOIN (VALUES ('overtime', 'READ'), ('overtime', 'WRITE')) AS f(k, a)
           WHERE al."isSystem" AND al.code IN ('SYSTEM_ADMIN', 'SUPER_ADMIN', 'ADMIN', 'MANAGER', 'STAFF')
           ON CONFLICT DO NOTHING"#,
    )
    .await;
    app.exec(&format!(
        r#"DELETE FROM "overtimeRequest" WHERE "companyId" = {0};
           DELETE FROM "overtimeApprover" WHERE "companyId" = {0};
           DELETE FROM "companyOvertimeSetting" WHERE "companyId" = {0};
           INSERT INTO "companyOvertimeSetting" ("companyId", enabled, "retroWindowDays", "createdAt", "updatedAt")
             VALUES ({0}, true, 7, now(), now());
           UPDATE "leaveRequest" SET status = 'REJECTED'
             WHERE "companyId" = {0} AND status = 'APPROVED' AND "endDate" >= current_date - 30;
           UPDATE company SET "timeZone" = 'Asia/Jakarta' WHERE id = {0};"#,
        c.id
    ))
    .await;
    set_approvers(&app, &c, &[c.admin.id, c.manager.id]).await;
    Some((app, c))
}

async fn set_approvers(app: &TestApp, c: &Company, ids: &[i32]) {
    app.exec(&format!(
        r#"DELETE FROM "overtimeApprover" WHERE "companyId" = {}"#,
        c.id
    ))
    .await;
    for id in ids {
        app.exec(&format!(
            r#"INSERT INTO "overtimeApprover" ("companyId", "userId") VALUES ({}, {id})"#,
            c.id
        ))
        .await;
    }
}

/// Satu `overtimeRequest` mentah; mengembalikan uuid.
async fn request(
    app: &TestApp,
    c: &Company,
    user: i32,
    status: &str,
    date: &str,
    sec: i32,
    deadline: &str,
) -> String {
    app.scalar(&format!(
        r#"WITH ins AS (
             INSERT INTO "overtimeRequest" ("companyId", "userId", "requestedById", kind, date, "requestedSec",
               reason, status, "retroWindowDays", "responseDeadline", "finalAt")
             VALUES ({c}, {user}, {user}, 'SELF', '{date}', {sec}, 'uji', '{status}', 7, '{deadline}', '{deadline}')
             RETURNING uuid::text)
           SELECT uuid FROM ins"#,
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

fn wire_keys(raw: &str, marker: &str, keys: &[&str]) -> Vec<usize> {
    let start = raw.find(marker).unwrap_or(0);
    let tail = &raw[start..];
    keys.iter()
        .map(|k| {
            tail.find(&format!("\"{k}\":"))
                .map_or(usize::MAX, |p| p + start)
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

fn code(v: &Value) -> &str {
    v["code"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn gates_auth_roles_and_uuid_pipe() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let id = RANDOM_V4;
    let routes: Vec<(&str, String)> = vec![
        ("GET", "/v1/overtime/approvals".into()),
        ("GET", "/v1/overtime/approvals/history".into()),
        ("GET", "/v1/overtime/approvals/quota".into()),
        ("POST", "/v1/overtime/approvals/orders".into()),
        ("GET", format!("/v1/overtime/approvals/{id}")),
        ("POST", format!("/v1/overtime/approvals/{id}/approve")),
        ("POST", format!("/v1/overtime/approvals/{id}/reject")),
    ];
    for (m, p) in &routes {
        let req = if *m == "GET" {
            app.get(p)
        } else {
            app.post(p).json(&json!({}))
        };
        req.send().await.assert_status(401);
        // STAFF tidak ada di @Roles.
        let req = if *m == "GET" {
            app.get(p)
        } else {
            app.post(p).json(&json!({}))
        };
        let res = req.token(&c.staff.token).send().await;
        res.assert_status(401);
        assert_eq!(
            res.json,
            json!({"message": "Access denied: insufficient permissions", "error": "Unauthorized", "statusCode": 401})
        );
    }

    // ParseUUIDPipe v4 (setelah gerbang, sebelum body).
    let bad = json!({"message": PIPE_400, "error": "Bad Request", "statusCode": 400});
    for p in [
        "/v1/overtime/approvals/abc",
        "/v1/overtime/approvals/orders",
        "/v1/overtime/approvals/6ba7b810-9dad-11d1-80b4-00c04fd430c8",
    ] {
        let res = app.get(p).token(&c.admin.token).send().await;
        res.assert_status(400);
        assert_eq!(res.json, bad, "{p}");
    }
    for p in [
        "/v1/overtime/approvals/abc/approve",
        "/v1/overtime/approvals/abc/reject",
    ] {
        let res = app
            .post(p)
            .token(&c.admin.token)
            .json(&json!({"reason": "alasan"}))
            .send()
            .await;
        res.assert_status(400);
        assert_eq!(res.json, bad, "{p}");
    }
    // id rusak + body rusak: pipe id menang.
    let res = app
        .post("/v1/overtime/approvals/abc/reject")
        .token(&c.admin.token)
        .json(&json!({}))
        .send()
        .await;
    assert_eq!(res.json, bad);
}

#[tokio::test]
async fn non_approver_admin_is_refused() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    set_approvers(&app, &c, &[c.manager.id]).await;
    let tomorrow = jkt_date(&app, 1).await;
    let uuid = request(
        &app,
        &c,
        c.staff.id,
        "PENDING_APPROVAL",
        &tomorrow,
        3600,
        "2099-01-01T00:00:00",
    )
    .await;
    let t = &c.admin.token;
    for p in [
        "/v1/overtime/approvals".to_owned(),
        "/v1/overtime/approvals/history".to_owned(),
        format!(
            "/v1/overtime/approvals/quota?userIds={}&date={tomorrow}&durationMinutes=60",
            c.staff.uuid
        ),
    ] {
        let res = app.get(&p).token(t).send().await;
        res.assert_status(403);
        assert_eq!(
            res.json,
            json!({"code": "OVERTIME_NOT_APPROVER", "message": "Anda bukan approver lembur di perusahaan ini."}),
            "{p}"
        );
    }
    for (p, body) in [
        (format!("/v1/overtime/approvals/{uuid}/approve"), json!({})),
        (
            format!("/v1/overtime/approvals/{uuid}/reject"),
            json!({"reason": "x"}),
        ),
        (
            "/v1/overtime/approvals/orders".to_owned(),
            json!({"userIds": [c.staff.uuid], "date": tomorrow, "durationMinutes": 60, "reason": "x"}),
        ),
    ] {
        let res = app.post(&p).token(t).json(&body).send().await;
        res.assert_status(403);
        assert_eq!(code(&res.json), "OVERTIME_NOT_APPROVER", "{p}");
    }
    // Detail: 404 (tak membocorkan keberadaan).
    let res = app
        .get(&format!("/v1/overtime/approvals/{uuid}"))
        .token(t)
        .send()
        .await;
    res.assert_status(404);
    assert_eq!(
        res.json,
        json!({"code": "OVERTIME_NOT_FOUND", "message": "Pengajuan lembur tidak ditemukan."})
    );
    // Status tak berubah.
    let status: String = app
        .scalar(&format!(
            r#"SELECT status::text FROM "overtimeRequest" WHERE uuid = '{uuid}'"#
        ))
        .await;
    assert_eq!(status, "PENDING_APPROVAL");
}

#[tokio::test]
async fn list_history_detail_and_query_validation() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let d1 = jkt_date(&app, 1).await;
    let d2 = jkt_date(&app, 2).await;
    let staff_req = request(
        &app,
        &c,
        c.staff.id,
        "PENDING_APPROVAL",
        &d2,
        3600,
        "2099-01-01T00:00:00",
    )
    .await;
    let own_req = request(
        &app,
        &c,
        c.admin.id,
        "PENDING_APPROVAL",
        &d1,
        3600,
        "2099-01-02T00:00:00",
    )
    .await;
    let done_req = request(
        &app,
        &c,
        c.staff2.id,
        "PENDING_APPROVAL",
        &d1,
        3600,
        "2099-01-01T00:00:00",
    )
    .await;
    app.exec(&format!(
        r#"UPDATE "overtimeRequest" SET status = 'REJECTED', "rejectReason" = 'x' WHERE uuid = '{done_req}'"#
    ))
    .await;

    let res = app
        .get("/v1/overtime/approvals")
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(200);
    let rows = res.data().as_array().unwrap().clone();
    // Default PENDING_APPROVAL, urut responseDeadline ASC.
    let ids: Vec<&str> = rows.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![staff_req.as_str(), own_req.as_str()]);
    assert_eq!(rows[0]["canDecide"], true);
    // Pengajuan sendiri dengan 2 approver: tak bisa diputuskan sendiri.
    assert_eq!(rows[1]["canDecide"], false);
    assert!(rows[0]["quota"]["dailyLimitMinutes"].is_number());
    assert!(rows[0]["dayType"].is_string());
    assert_eq!(rows[0]["durationMinutes"], 60);
    assert_eq!(
        res.json["meta"],
        json!({"total": 2, "page": 1, "limit": 20, "totalPages": 1})
    );
    assert_eq!(res.json["readOnly"], false);
    let raw = res.text();
    assert_wire_order(&raw, "{", &["data", "meta", "readOnly"]);
    assert_wire_order(
        &raw,
        "\"data\"",
        &[
            "id",
            "kind",
            "status",
            "date",
            "durationMinutes",
            "employee",
            "requestedBy",
            "decidedBy",
            "responseDeadline",
            "dayType",
            "createdAt",
            "quota",
            "canDecide",
        ],
    );

    // Filter userId + halaman.
    let res = app
        .get(&format!(
            "/v1/overtime/approvals?userId={}&limit=1&page=1",
            c.staff.uuid
        ))
        .token(&c.manager.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.data().as_array().unwrap().len(), 1);
    assert_eq!(res.json["meta"]["total"], 1);
    // Approver lain (manager) boleh memutuskan pengajuan admin.
    let res = app
        .get("/v1/overtime/approvals?limit=1&page=2")
        .token(&c.manager.token)
        .send()
        .await;
    assert_eq!(res.data()[0]["id"], own_req.as_str());
    assert_eq!(res.data()[0]["canDecide"], true);
    assert_eq!(res.json["meta"]["totalPages"], 2);

    // Riwayat: semua status, date DESC, canDecide false.
    let res = app
        .get("/v1/overtime/approvals/history")
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(200);
    let rows = res.data().as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["id"], staff_req.as_str());
    assert!(rows.iter().all(|r| r["canDecide"] == false));
    let res = app
        .get("/v1/overtime/approvals/history?status=REJECTED&status=REJECTED")
        .token(&c.admin.token)
        .send()
        .await;
    assert_eq!(res.data().as_array().unwrap().len(), 1);
    assert_eq!(res.data()[0]["rejectReason"], "x");

    // Detail.
    let res = app
        .get(&format!("/v1/overtime/approvals/{staff_req}"))
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.data()["id"], staff_req.as_str());
    assert_eq!(res.data()["canDecide"], true);
    assert!(res.data()["dayType"].is_string());
    assert!(res.data().get("quota").is_none());
    assert_wire_order(&res.text(), "\"data\"", &["id", "createdAt", "canDecide"]);
    let res = app
        .get(&format!("/v1/overtime/approvals/{RANDOM_V4}"))
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(404);
    assert_eq!(code(&res.json), "OVERTIME_NOT_FOUND");

    // Validasi query.
    let res = app
        .get("/v1/overtime/approvals?limit=x&userId=12")
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json,
        json!({"message": ["userId must be a UUID", "limit must not be greater than 100",
                           "limit must not be less than 1", "limit must be an integer number"],
               "error": "Bad Request", "statusCode": 400})
    );
    for qs in [
        "status=NOPE",
        "from=2026/10/01",
        "to=2026-04-31",
        "page=0",
        "limit=101",
    ] {
        app.get(&format!("/v1/overtime/approvals/history?{qs}"))
            .token(&c.admin.token)
            .send()
            .await
            .assert_status(400);
    }
    let res = app
        .get("/v1/overtime/approvals?from=2026-10-31&to=2026-10-01")
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json,
        json!({"message": "Tanggal \"from\" tidak boleh setelah \"to\".", "error": "Bad Request", "statusCode": 400})
    );

    // Fitur nonaktif: tetap terbaca, readOnly, canDecide false.
    app.exec(&format!(
        r#"UPDATE "companyOvertimeSetting" SET enabled = false WHERE "companyId" = {}"#,
        c.id
    ))
    .await;
    let res = app
        .get("/v1/overtime/approvals")
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["readOnly"], true);
    assert!(
        res.data()
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["canDecide"] == false)
    );
    let res = app
        .get(&format!("/v1/overtime/approvals/{staff_req}"))
        .token(&c.admin.token)
        .send()
        .await;
    assert_eq!(res.data()["canDecide"], false);
    let res = app
        .post(&format!("/v1/overtime/approvals/{staff_req}/approve"))
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(403);
    assert_eq!(
        res.json,
        json!({"code": "OVERTIME_DISABLED", "message": "Fitur lembur sedang dinonaktifkan."})
    );
}

#[tokio::test]
async fn approve_flow_quota_conflicts_and_retro_compute() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let d1 = jkt_date(&app, 1).await;
    // 13 jam > batas harian hari kerja (4 jam) dan hari libur (12 jam).
    let big = request(
        &app,
        &c,
        c.staff.id,
        "PENDING_APPROVAL",
        &d1,
        46_800,
        "2099-01-01T00:00:00",
    )
    .await;
    let path = format!("/v1/overtime/approvals/{big}/approve");

    // Body tidak valid.
    let res = app
        .post(&path)
        .token(&c.admin.token)
        .json(&json!({"confirmOverQuota": "true"}))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!(["confirmOverQuota must be a boolean value"])
    );

    // Kuota terlampaui tanpa konfirmasi → 409 + details evaluasi.
    let res = app.post(&path).token(&c.admin.token).send().await;
    res.assert_status(409);
    assert_eq!(code(&res.json), "OVERTIME_QUOTA_CONFIRMATION_REQUIRED");
    assert_eq!(res.json["details"]["exceedsDaily"], true);
    assert_eq!(res.json["details"]["requestedMinutes"], 780);

    // Dengan konfirmasi → APPROVED + snapshot.
    let res = app
        .post(&path)
        .token(&c.admin.token)
        .json(&json!({"confirmOverQuota": true}))
        .send()
        .await;
    res.assert_status(200);
    let d = res.data();
    assert_eq!(d["status"], "APPROVED");
    assert_eq!(d["decidedBy"]["id"], c.admin.uuid.as_str());
    assert!(d["decidedAt"].is_string());
    assert!(d["dayType"].is_string());
    assert_eq!(d["exceedsDailyLimit"], true);
    assert_eq!(d["exceedsLimit"], true);
    assert!(res.json.get("quota").is_none());
    let snap: (Option<i32>, Option<i16>, bool) = sqlx::query_as(
        r#"SELECT "baseNormalSec", "patternWorkDays", "approvedAt" IS NOT NULL
           FROM "overtimeRequest" WHERE uuid = $1::uuid"#,
    )
    .bind(&big)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert!(snap.0.is_some() && snap.1.is_some() && snap.2);

    // Sudah diputuskan → 409 STATE_CONFLICT (approve & reject).
    let res = app
        .post(&path)
        .token(&c.manager.token)
        .json(&json!({"confirmOverQuota": true}))
        .send()
        .await;
    res.assert_status(409);
    assert_eq!(
        res.json,
        json!({"code": "OVERTIME_STATE_CONFLICT", "message": "Status pengajuan lembur sudah berubah. Muat ulang lalu coba lagi."})
    );
    let res = app
        .post(&format!("/v1/overtime/approvals/{big}/reject"))
        .token(&c.manager.token)
        .json(&json!({"reason": "x"}))
        .send()
        .await;
    res.assert_status(409);

    // Pengajuan sendiri dengan 2 approver → 403.
    let own = request(
        &app,
        &c,
        c.admin.id,
        "PENDING_APPROVAL",
        &jkt_date(&app, 2).await,
        3600,
        "2099-01-01T00:00:00",
    )
    .await;
    let res = app
        .post(&format!("/v1/overtime/approvals/{own}/approve"))
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(403);
    assert_eq!(code(&res.json), "OVERTIME_SELF_DECISION");

    // Tenggat lewat → 409.
    let late = request(
        &app,
        &c,
        c.staff2.id,
        "PENDING_APPROVAL",
        &jkt_date(&app, 3).await,
        3600,
        "2020-01-01T00:00:00",
    )
    .await;
    let res = app
        .post(&format!("/v1/overtime/approvals/{late}/approve"))
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(409);
    assert_eq!(code(&res.json), "OVERTIME_DEADLINE_PASSED");

    // Tak ada di company → 404.
    let res = app
        .post(&format!("/v1/overtime/approvals/{RANDOM_V4}/approve"))
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(404);

    // Persetujuan retroaktif: tanggal lampau → dihitung di tempat → COMPLETED.
    let past = request(
        &app,
        &c,
        c.staff2.id,
        "PENDING_APPROVAL",
        &jkt_date(&app, -2).await,
        3600,
        "2099-01-01T00:00:00",
    )
    .await;
    let res = app
        .post(&format!("/v1/overtime/approvals/{past}/approve"))
        .token(&c.manager.token)
        .json(&json!({"confirmOverQuota": true}))
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.data()["status"], "COMPLETED");
    assert!(res.data()["workedMinutes"].is_number());
    assert!(res.data()["computedAt"].is_string());
}

#[tokio::test]
async fn reject_flow() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let uuid = request(
        &app,
        &c,
        c.staff.id,
        "PENDING_APPROVAL",
        &jkt_date(&app, 1).await,
        3600,
        "2099-01-01T00:00:00",
    )
    .await;
    let path = format!("/v1/overtime/approvals/{uuid}/reject");
    let res = app.post(&path).token(&c.admin.token).send().await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "reason must be shorter than or equal to 500 characters",
            "reason should not be empty",
            "reason must be a string"
        ])
    );
    app.post(&path)
        .token(&c.admin.token)
        .json(&json!({"reason": "   "}))
        .send()
        .await
        .assert_status(400);

    let res = app
        .post(&path)
        .token(&c.admin.token)
        .json(&json!({"reason": "  Tidak perlu  "}))
        .send()
        .await;
    res.assert_status(200);
    let d = res.data();
    assert_eq!(d["status"], "REJECTED");
    assert_eq!(d["rejectReason"], "Tidak perlu");
    assert_eq!(d["decidedBy"]["id"], c.admin.uuid.as_str());
    assert_eq!(d["dayType"], Value::Null);
    assert_eq!(d["normalMinutes"], Value::Null);
}

#[tokio::test]
async fn orders_create_refusals_and_errors() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let date = jkt_date(&app, 3).await;
    let url = "/v1/overtime/approvals/orders";

    // 400 validasi DTO.
    let res = app
        .post(url)
        .token(&c.admin.token)
        .json(&json!({"userIds": [], "date": date, "durationMinutes": 60, "reason": "x"}))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!(["userIds must contain at least 1 elements"])
    );

    // Tanggal lampau → 422 seluruh panggilan.
    let yesterday = jkt_date(&app, -1).await;
    let today = jkt_date(&app, 0).await;
    let res = app
        .post(url)
        .token(&c.admin.token)
        .json(&json!({"userIds": [c.staff.uuid], "date": yesterday, "durationMinutes": 60, "reason": "x"}))
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(
        res.json,
        json!({"code": "OVERTIME_ORDER_PAST_DATE",
               "message": "Perintah lembur hanya untuk hari ini atau tanggal ke depan.",
               "details": {"localToday": today}})
    );

    // Campuran: dibuat (staff2, staff), SELF, ROLE_TOO_HIGH, NOT_ELIGIBLE.
    let body = json!({
        "userIds": [c.staff2.uuid, c.admin.uuid, c.super_admin.uuid, RANDOM_V4, c.staff.uuid],
        "date": date, "durationMinutes": 90, "reason": "  Stock opname  "
    });
    let res = app.post(url).token(&c.admin.token).json(&body).send().await;
    res.assert_status(201);
    let created = res.data()["created"].as_array().unwrap().clone();
    assert_eq!(created.len(), 2);
    assert_eq!(created[0]["employee"]["id"], c.staff2.uuid.as_str());
    assert_eq!(created[1]["employee"]["id"], c.staff.uuid.as_str());
    assert_eq!(created[0]["kind"], "ORDER");
    assert_eq!(created[0]["status"], "PENDING_EMPLOYEE");
    assert_eq!(created[0]["requestedBy"]["id"], c.admin.uuid.as_str());
    assert_eq!(created[0]["reason"], "Stock opname");
    assert_eq!(created[0]["durationMinutes"], 90);
    assert!(created[0]["dayType"].is_string());
    assert_eq!(
        res.data()["refused"],
        json!([
            {"userId": c.admin.uuid, "code": "OVERTIME_ORDER_SELF",
             "message": "Anda tidak dapat memerintah lembur untuk diri sendiri."},
            {"userId": c.super_admin.uuid, "code": "ROLE_TOO_HIGH",
             "message": "Jabatan karyawan lebih tinggi dari jabatan Anda."},
            {"userId": RANDOM_V4, "code": "OVERTIME_EMPLOYEE_NOT_ELIGIBLE",
             "message": "Karyawan tidak dapat diberi lembur di perusahaan ini."}
        ])
    );
    assert_wire_order(&res.text(), "\"data\"", &["created", "refused"]);
    // responseDeadline = akhir tanggal lokal (Jakarta = UTC+7 → 16:59:59.999Z).
    let deadline = created[0]["responseDeadline"].as_str().unwrap();
    assert!(deadline.ends_with("T16:59:59.999Z"), "{deadline}");
    let (retro, kind): (i32, String) = sqlx::query_as(
        r#"SELECT "retroWindowDays", kind::text FROM "overtimeRequest"
           WHERE uuid = $1::uuid"#,
    )
    .bind(created[0]["id"].as_str().unwrap())
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!((retro, kind.as_str()), (7, "ORDER"));

    // Ulang: tanggal terpakai → DATE_TAKEN per entri (201).
    let res = app
        .post(url)
        .token(&c.manager.token)
        .json(
            &json!({"userIds": [c.staff.uuid], "date": date, "durationMinutes": 60, "reason": "x"}),
        )
        .send()
        .await;
    res.assert_status(201);
    assert_eq!(res.data()["created"], json!([]));
    assert_eq!(res.data()["refused"][0]["code"], "OVERTIME_DATE_TAKEN");

    // MANAGER tak boleh memerintah ADMIN.
    let res = app
        .post(url)
        .token(&c.manager.token)
        .json(
            &json!({"userIds": [c.admin.uuid], "date": date, "durationMinutes": 60, "reason": "x"}),
        )
        .send()
        .await;
    res.assert_status(201);
    assert_eq!(res.data()["refused"][0]["code"], "ROLE_TOO_HIGH");

    // Cuti sehari penuh → ON_LEAVE; durasi + normal > 24 jam → DURATION_EXCEEDS_DAY.
    let leave_date = jkt_date(&app, 10).await;
    app.exec(&format!(
        r#"INSERT INTO "leaveRequest" ("userId", "companyId", "startDate", "endDate", "totalDays", reason, status)
           VALUES ({}, {}, '{leave_date}', '{leave_date}', 1, 'uji', 'APPROVED')"#,
        c.staff2.id, c.id
    ))
    .await;
    let res = app
        .post(url)
        .token(&c.admin.token)
        .json(&json!({"userIds": [c.staff2.uuid], "date": leave_date, "durationMinutes": 60, "reason": "x"}))
        .send()
        .await;
    res.assert_status(201);
    assert_eq!(
        res.data()["refused"][0]["code"],
        "OVERTIME_EMPLOYEE_ON_LEAVE"
    );

    // Fitur nonaktif → 403 tanpa menulis.
    app.exec(&format!(
        r#"UPDATE "companyOvertimeSetting" SET enabled = false WHERE "companyId" = {}"#,
        c.id
    ))
    .await;
    let res = app
        .post(url)
        .token(&c.admin.token)
        .json(&json!({"userIds": [c.manager.uuid], "date": jkt_date(&app, 4).await, "durationMinutes": 60, "reason": "x"}))
        .send()
        .await;
    res.assert_status(403);
    assert_eq!(code(&res.json), "OVERTIME_DISABLED");
}

#[tokio::test]
async fn order_quota_preview() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let date = jkt_date(&app, 5).await;
    let res = app
        .get(&format!(
            "/v1/overtime/approvals/quota?userIds={},{},{},{}&date={date}&durationMinutes=60",
            c.staff.uuid,
            c.admin.uuid,
            c.super_admin.uuid,
            c.staff.uuid.to_uppercase()
        ))
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(200);
    let data = res.data().as_array().unwrap();
    assert_eq!(data.len(), 3);
    assert_eq!(data[0]["userId"], c.staff.uuid.as_str());
    assert_eq!(data[0]["refusal"], Value::Null);
    assert_eq!(data[0]["quota"]["requestedMinutes"], 60);
    assert_eq!(data[0]["quota"]["date"], date.as_str());
    assert_eq!(data[1]["refusal"], "OVERTIME_ORDER_SELF");
    assert_eq!(data[1]["quota"], Value::Null);
    assert_eq!(data[2]["refusal"], "ROLE_TOO_HIGH");
    assert_wire_order(&res.text(), "\"data\"", &["userId", "quota", "refusal"]);

    // Tanggal terpakai → DATE_TAKEN di pratinjau.
    request(
        &app,
        &c,
        c.staff.id,
        "PENDING_APPROVAL",
        &date,
        3600,
        "2099-01-01T00:00:00",
    )
    .await;
    let res = app
        .get(&format!(
            "/v1/overtime/approvals/quota?userIds={}&date={date}&durationMinutes=60",
            c.staff.uuid
        ))
        .token(&c.manager.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.data()[0]["refusal"], "OVERTIME_DATE_TAKEN");

    // Validasi query.
    let res = app
        .get("/v1/overtime/approvals/quota")
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "each value in userIds must be a UUID",
            "userIds must contain no more than 100 elements",
            "userIds must contain at least 1 elements",
            "userIds must be an array",
            "date harus tanggal YYYY-MM-DD yang valid",
            "durationMinutes must not be greater than 1440",
            "durationMinutes must not be less than 1",
            "durationMinutes must be an integer number"
        ])
    );

    // Tanggal lampau → 422.
    let res = app
        .get(&format!(
            "/v1/overtime/approvals/quota?userIds={}&date={}&durationMinutes=60",
            c.staff.uuid,
            jkt_date(&app, -1).await
        ))
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(code(&res.json), "OVERTIME_ORDER_PAST_DATE");
}
