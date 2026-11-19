//! Uji integrasi 7 route pengajuan lembur karyawan (`/v1/overtime/requests*`) lewat HTTP
//! (router dengan gerbang lengkap) terhadap Postgres & Redis nyata.
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-overtime --test it_ot_requests`.
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

const BASE: &str = "/v1/overtime/requests";

fn build(state: &AppState) -> Router {
    mz_overtime::hr::requests::router(state)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

/// Aplikasi + company: izin fitur `overtime`, setelan aktif (jendela 7), approver
/// SUPER_ADMIN + MANAGER, tanpa pengajuan; zona Asia/Jakarta.
async fn setup() -> Option<(TestApp, Company)> {
    let app = TestApp::new(build).await?;
    let c = app.company().await;
    app.exec(
        r#"INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
           SELECT al.id, f.k, f.a::"PermissionAction" FROM "accessLevel" al
           CROSS JOIN (VALUES ('overtime', 'READ'), ('overtime', 'WRITE')) AS f(k, a)
           WHERE al."isSystem" AND al.code IN ('SYSTEM_ADMIN', 'SUPER_ADMIN', 'ADMIN', 'MANAGER', 'STAFF', 'GUEST')
           ON CONFLICT DO NOTHING"#,
    )
    .await;
    app.exec(&format!(
        r#"DELETE FROM "overtimeRequest" WHERE "companyId" = {c};
           DELETE FROM "overtimeApprover" WHERE "companyId" = {c};
           DELETE FROM "companyOvertimeSetting" WHERE "companyId" = {c};
           INSERT INTO "companyOvertimeSetting" ("companyId", enabled, "retroWindowDays", "createdAt", "updatedAt")
           VALUES ({c}, true, 7, now(), now());
           INSERT INTO "overtimeApprover" ("companyId", "userId", "createdAt")
           VALUES ({c}, {sa}, now()), ({c}, {mg}, now());
           UPDATE company SET "timeZone" = 'Asia/Jakarta' WHERE id = {c};"#,
        c = c.id,
        sa = c.super_admin.id,
        mg = c.manager.id,
    ))
    .await;
    Some((app, c))
}

/// Tanggal lokal Jakarta + `days`.
async fn jkt_date(app: &TestApp, days: i32) -> String {
    app.scalar(&format!(
        "SELECT ((now() AT TIME ZONE 'Asia/Jakarta')::date + {days})::text"
    ))
    .await
}

/// Hari kerja (Senin–Jumat, pola bawaan) ke-`n` mulai besok (lokal Jakarta).
async fn workday(app: &TestApp, n: i32) -> String {
    app.scalar(&format!(
        r#"SELECT d::date::text FROM generate_series(
             (now() AT TIME ZONE 'Asia/Jakarta')::date + 1,
             (now() AT TIME ZONE 'Asia/Jakarta')::date + 120, interval '1 day') d
           WHERE extract(isodow FROM d) < 6 ORDER BY d OFFSET {n} LIMIT 1"#
    ))
    .await
}

/// `overtimeRequest` mentah; mengembalikan uuid.
#[allow(clippy::too_many_arguments)]
async fn raw_request(
    app: &TestApp,
    c: &Company,
    user: i32,
    requested_by: i32,
    kind: &str,
    status: &str,
    date: &str,
    minutes: i32,
    deadline_sql: &str,
) -> String {
    app.scalar(&format!(
        r#"WITH ins AS (
             INSERT INTO "overtimeRequest" ("companyId", "userId", "requestedById", kind, date, "requestedSec",
               reason, status, "retroWindowDays", "responseDeadline", "finalAt")
             VALUES ({c}, {user}, {requested_by}, '{kind}', '{date}', {sec}, 'uji', '{status}', 7,
                     {deadline_sql}, {deadline_sql})
             RETURNING uuid)
           SELECT uuid::text FROM ins"#,
        c = c.id,
        sec = minutes * 60,
    ))
    .await
}

/// Posisi kemunculan pertama `"k":` harus naik (urutan kunci di kabel = Node).
fn assert_key_order(raw: &str, keys: &[&str]) {
    let pos: Vec<usize> = keys
        .iter()
        .map(|k| {
            raw.find(&format!("\"{k}\":"))
                .unwrap_or_else(|| panic!("kunci {k} tak ada di {raw}"))
        })
        .collect();
    let mut sorted = pos.clone();
    sorted.sort_unstable();
    assert_eq!(pos, sorted, "urutan kunci {keys:?} di {raw}");
}

fn ot_error(code: &str, message: &str) -> Value {
    json!({"code": code, "message": message})
}

const ENTITY_KEYS: &[&str] = &[
    "id",
    "kind",
    "status",
    "date",
    "durationMinutes",
    "reason",
    "employee",
    "requestedBy",
    "decidedBy",
    "decidedAt",
    "rejectReason",
    "cancelReason",
    "cancelReasonLabel",
    "responseDeadline",
    "dayType",
    "normalMinutes",
    "workedMinutes",
    "overtimeMinutes",
    "overtimeSeconds",
    "computedAt",
    "isFinal",
    "dailyLimitMinutes",
    "exceedsLimit",
    "exceedsDailyLimit",
    "exceedsWeeklyLimit",
    "createdAt",
];

#[tokio::test]
async fn gates_roles_and_uuid_pipe() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let id = "5d2c8a9b-7e34-4f1a-b6c2-9a8e7d6c5b02";
    let routes: Vec<(&str, String)> = vec![
        ("GET", format!("{BASE}/me")),
        ("GET", format!("{BASE}/quota")),
        ("POST", BASE.to_owned()),
        ("GET", format!("{BASE}/{id}")),
        ("POST", format!("{BASE}/{id}/cancel")),
        ("POST", format!("{BASE}/{id}/accept")),
        ("POST", format!("{BASE}/{id}/reject")),
    ];
    for (m, p) in &routes {
        let req = if *m == "GET" { app.get(p) } else { app.post(p) };
        req.send().await.assert_status(401);
    }

    // GUEST (tak ada di @Roles) → 401 RolesGuard. Ubah satu STAFF lain menjadi GUEST.
    let guest: i32 = app
        .scalar(&format!(
            r#"WITH g AS (
                 SELECT ual.id FROM "userAccessLevel" ual
                 JOIN "accessLevel" al ON al.id = ual."accessLevelId"
                 JOIN users u ON u.id = ual."userId"
                 WHERE ual."companyId" = {c} AND ual.status = 1 AND u.status = 1
                   AND al.code IN ('STAFF', 'GUEST') AND al."isSystem"
                   AND u.id NOT IN ({s1}, {s2})
                   AND NOT EXISTS (SELECT 1 FROM "userCompany" o WHERE o."userId" = u.id
                                   AND o.status = 1 AND o."companyId" <> {c})
                 ORDER BY u.id LIMIT 1)
               UPDATE "userAccessLevel" SET "accessLevelId" = (SELECT id FROM "accessLevel" WHERE code = 'GUEST' AND "isSystem")
               WHERE id IN (SELECT id FROM g) RETURNING "userId""#,
            c = c.id,
            s1 = c.staff.id,
            s2 = c.staff2.id,
        ))
        .await;
    let guest_token = app.token(guest);
    for (m, p) in &routes {
        let req = if *m == "GET" { app.get(p) } else { app.post(p) };
        let res = req.token(&guest_token).send().await;
        res.assert_status(401);
        assert_eq!(
            res.json,
            json!({"message": "Access denied: insufficient permissions", "error": "Unauthorized", "statusCode": 401})
        );
    }

    // ParseUUIDPipe v4 → 400, sebelum validasi body.
    let uuid_err = json!({"message": "Validation failed (uuid v 4 is expected)", "error": "Bad Request", "statusCode": 400});
    for (m, p) in [
        ("GET", format!("{BASE}/not-a-uuid")),
        (
            "GET",
            format!("{BASE}/6ba7b810-9dad-11d1-80b4-00c04fd430c8"),
        ),
        ("POST", format!("{BASE}/123/cancel")),
        ("POST", format!("{BASE}/abc/accept")),
        ("POST", format!("{BASE}/abc/reject")),
    ] {
        let req = if m == "GET" {
            app.get(&p)
        } else {
            app.post(&p).json(&json!({"confirmOverQuota": null}))
        };
        let res = req.token(&c.staff.token).send().await;
        res.assert_status(400);
        assert_eq!(res.json, uuid_err, "{m} {p}");
    }
    // `me`/`quota` tidak tertangkap `:id`.
    app.get(&format!("{BASE}/me"))
        .token(&c.staff.token)
        .send()
        .await
        .assert_status(200);
}

#[tokio::test]
async fn quota_preview_and_validation() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let w = workday(&app, 0).await;
    let res = app
        .get(&format!("{BASE}/quota?date={w}&durationMinutes=180"))
        .token(&c.staff.token)
        .send()
        .await;
    res.assert_status(200);
    let d = res.data();
    assert_eq!(d["date"], w);
    assert_eq!(d["dayType"], "WORKDAY");
    assert_eq!(d["patternWorkDays"], 5);
    assert_eq!(d["normalMinutes"], 480);
    assert_eq!(d["dailyLimitMinutes"], 240);
    assert_eq!(d["requestedMinutes"], 180);
    assert_eq!(d["weeklyLimitMinutes"], 1080);
    assert_eq!(d["exceedsDaily"], false);
    assert_eq!(d["messages"], json!([]));
    assert_key_order(
        &res.text(),
        &[
            "data",
            "date",
            "dayType",
            "patternWorkDays",
            "normalMinutes",
            "dailyLimitMinutes",
            "requestedMinutes",
            "dailyRemainingMinutes",
            "weeklyLimitMinutes",
            "weeklyUsedMinutes",
            "weeklyPendingMinutes",
            "weeklyRemainingMinutes",
            "exceedsDaily",
            "exceedsWeekly",
            "messages",
        ],
    );
    let res = app
        .get(&format!("{BASE}/quota?date={w}&durationMinutes=300"))
        .token(&c.staff.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.data()["exceedsDaily"], true);
    assert_eq!(
        res.data()["messages"],
        json!(["Lembur 5 jam melebihi batas harian 4 jam."])
    );

    let res = app
        .get(&format!("{BASE}/quota"))
        .token(&c.staff.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json,
        json!({
            "message": [
                "date harus tanggal YYYY-MM-DD yang valid",
                "durationMinutes must not be greater than 1440",
                "durationMinutes must not be less than 1",
                "durationMinutes must be an integer number"
            ],
            "error": "Bad Request",
            "statusCode": 400
        })
    );
    let res = app
        .get(&format!("{BASE}/quota?date=2026-13-01&durationMinutes=1.5"))
        .token(&c.staff.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "date harus tanggal YYYY-MM-DD yang valid",
            "durationMinutes must be an integer number"
        ])
    );
}

#[tokio::test]
async fn submit_flow_and_errors() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let w0 = workday(&app, 0).await;
    let w1 = workday(&app, 1).await;
    let post = |body: Value| app.post(BASE).token(&c.staff.token).json(&body).send();

    let res = post(json!({"date": w0, "durationMinutes": 120, "reason": "  Closing bulanan  ", "kind": "ORDER"})).await;
    res.assert_status(201);
    let d = res.data().clone();
    assert_eq!(d["kind"], "SELF");
    assert_eq!(d["status"], "PENDING_APPROVAL");
    assert_eq!(d["date"], w0);
    assert_eq!(d["durationMinutes"], 120);
    assert_eq!(d["reason"], "Closing bulanan");
    assert_eq!(d["employee"]["id"], c.staff.uuid);
    assert_eq!(d["requestedBy"]["id"], c.staff.uuid);
    assert_eq!(d["decidedBy"], Value::Null);
    assert_eq!(d["dayType"], "WORKDAY");
    assert_eq!(d["normalMinutes"], 480);
    assert_eq!(d["isFinal"], false);
    assert_eq!(d["cancelReasonLabel"], Value::Null);
    // Tenggat = akhir (tanggal + 7) lokal Jakarta = 16:59:59.999Z.
    let deadline_day: String = app
        .scalar(&format!("SELECT ('{w0}'::date + 7)::text"))
        .await;
    assert_eq!(
        d["responseDeadline"],
        format!("{deadline_day}T16:59:59.999Z")
    );
    let mut keys = vec!["data"];
    keys.extend_from_slice(ENTITY_KEYS);
    assert_key_order(&res.text(), &keys);
    let (retro, final_eq, upd): (i32, bool, bool) = sqlx::query_as(
        r#"SELECT "retroWindowDays", "finalAt" = "responseDeadline", "updatedAt" = "createdAt"
           FROM "overtimeRequest" WHERE uuid = $1::uuid"#,
    )
    .bind(d["id"].as_str().unwrap())
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!((retro, final_eq, upd), (7, true, true));

    // T1: tanggal yang sama → 409 OVERTIME_DATE_TAKEN.
    let res = post(json!({"date": w0, "durationMinutes": 60, "reason": "lagi"})).await;
    res.assert_status(409);
    assert_eq!(
        res.json,
        ot_error(
            "OVERTIME_DATE_TAKEN",
            "Sudah ada pengajuan lembur aktif di tanggal ini."
        )
    );

    // T3: melebihi kuota harian tanpa konfirmasi → 409 + evaluasi; tak ada yang ditulis.
    let res = post(json!({"date": w1, "durationMinutes": 300, "reason": "banyak"})).await;
    res.assert_status(409);
    assert_eq!(res.json["code"], "OVERTIME_QUOTA_CONFIRMATION_REQUIRED");
    assert_eq!(
        res.json["message"],
        "Lembur ini melebihi batas kuota. Konfirmasi untuk tetap melanjutkan."
    );
    assert_eq!(res.json["details"]["exceedsDaily"], true);
    assert_eq!(res.json["details"]["date"], w1);
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "overtimeRequest" WHERE "companyId" = {} AND date = '{w1}'"#,
            c.id
        ))
        .await;
    assert_eq!(n, 0);
    let res = post(
        json!({"date": w1, "durationMinutes": 300, "reason": "banyak", "confirmOverQuota": true}),
    )
    .await;
    res.assert_status(201);

    // FX-7.3: 24 jam lembur + 8 jam normal > 24 jam → 400.
    let w2 = workday(&app, 2).await;
    let res = post(json!({"date": w2, "durationMinutes": 1440, "reason": "x"})).await;
    res.assert_status(400);
    assert_eq!(
        res.json,
        ot_error(
            "OVERTIME_DURATION_EXCEEDS_DAY",
            "Lembur 24 jam + jam normal 8 jam melebihi 24 jam."
        )
    );

    // Jendela retro: hari ini − 8 → 422 + details.
    let old = jkt_date(&app, -8).await;
    let earliest = jkt_date(&app, -7).await;
    let res = post(json!({"date": old, "durationMinutes": 60, "reason": "x"})).await;
    res.assert_status(422);
    assert_eq!(
        res.json,
        json!({
            "code": "OVERTIME_RETRO_WINDOW_PASSED",
            "message": "Tanggal ini sudah melewati batas pengajuan mundur (7 hari).",
            "details": {"earliestDate": earliest, "retroWindowDays": 7}
        })
    );
    assert_key_order(&res.text(), &["earliestDate", "retroWindowDays"]);

    // Validasi body (urutan properti + dekorator bawah→atas).
    let res = post(json!({})).await;
    res.assert_status(400);
    assert_eq!(
        res.json,
        json!({
            "message": [
                "date harus tanggal YYYY-MM-DD yang valid",
                "durationMinutes must not be greater than 1440",
                "durationMinutes must not be less than 1",
                "durationMinutes must be an integer number",
                "reason must be shorter than or equal to 500 characters",
                "reason should not be empty",
                "reason must be a string"
            ],
            "error": "Bad Request",
            "statusCode": 400
        })
    );
    let res =
        post(json!({"date": w2, "durationMinutes": 60, "reason": "   ", "confirmOverQuota": null}))
            .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "reason should not be empty",
            "confirmOverQuota must be a boolean value"
        ])
    );

    // Tanpa approver efektif → 422 OVERTIME_NO_APPROVER.
    app.exec(&format!(
        r#"DELETE FROM "overtimeApprover" WHERE "companyId" = {}"#,
        c.id
    ))
    .await;
    let res = post(json!({"date": w2, "durationMinutes": 60, "reason": "x"})).await;
    res.assert_status(422);
    assert_eq!(
        res.json,
        ot_error(
            "OVERTIME_NO_APPROVER",
            "Belum ada approver lembur. Minta admin memilih approver di Pengaturan Lembur."
        )
    );
}

#[tokio::test]
async fn list_mine_and_get_one() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let w0 = workday(&app, 0).await;
    let w1 = workday(&app, 1).await;
    let future = "now() + interval '7 days'";
    let a = raw_request(
        &app,
        &c,
        c.staff.id,
        c.staff.id,
        "SELF",
        "PENDING_APPROVAL",
        &w0,
        60,
        future,
    )
    .await;
    let b = raw_request(
        &app,
        &c,
        c.staff.id,
        c.super_admin.id,
        "ORDER",
        "APPROVED",
        &w1,
        90,
        future,
    )
    .await;
    let other = raw_request(
        &app,
        &c,
        c.staff2.id,
        c.staff2.id,
        "SELF",
        "PENDING_APPROVAL",
        &w0,
        60,
        future,
    )
    .await;

    let res = app
        .get(&format!("{BASE}/me"))
        .token(&c.staff.token)
        .send()
        .await;
    res.assert_status(200);
    let ids: Vec<&str> = res.json["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![b.as_str(), a.as_str()], "urut date desc");
    assert_eq!(
        res.json["meta"],
        json!({"total": 2, "page": 1, "limit": 20, "totalPages": 1})
    );
    assert_key_order(&res.text(), &["data", "meta"]);
    assert_key_order(&res.text(), &["total", "page", "limit", "totalPages"]);
    // Baris tanpa snapshot diberi hari live.
    assert_eq!(res.json["data"][1]["dayType"], "WORKDAY");
    assert_eq!(res.json["data"][1]["normalMinutes"], 480);

    let res = app
        .get(&format!(
            "{BASE}/me?status=REJECTED&status=APPROVED&from={w1}&to={w1}"
        ))
        .token(&c.staff.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["meta"]["total"], 1);
    assert_eq!(res.json["data"][0]["id"], b);
    assert_eq!(res.json["data"][0]["kind"], "ORDER");
    assert_eq!(res.json["data"][0]["requestedBy"]["id"], c.super_admin.uuid);

    let res = app
        .get(&format!("{BASE}/me?page=2&limit=1"))
        .token(&c.staff.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["data"][0]["id"], a);
    assert_eq!(
        res.json["meta"],
        json!({"total": 2, "page": 2, "limit": 1, "totalPages": 2})
    );

    let res = app
        .get(&format!("{BASE}/me?status=DONE&limit=101"))
        .token(&c.staff.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "each value in status must be one of the following values: PENDING_APPROVAL, PENDING_EMPLOYEE, APPROVED, REJECTED, CANCELLED, COMPLETED",
            "limit must not be greater than 100"
        ])
    );

    // Detail: milik sendiri (karyawan) dan sebagai pengaju ORDER.
    let res = app
        .get(&format!("{BASE}/{a}"))
        .token(&c.staff.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.data()["id"], a);
    assert_eq!(res.data()["normalMinutes"], 480);
    app.get(&format!("{BASE}/{b}"))
        .token(&c.super_admin.token)
        .send()
        .await
        .assert_status(200);
    // Milik orang lain / tak ada → 404.
    let not_found = ot_error("OVERTIME_NOT_FOUND", "Pengajuan lembur tidak ditemukan.");
    for (token, id) in [
        (&c.staff.token, other.as_str()),
        (&c.staff2.token, a.as_str()),
        (&c.manager.token, b.as_str()),
        (&c.staff.token, "5d2c8a9b-7e34-4f1a-b6c2-9a8e7d6c5b02"),
    ] {
        let res = app.get(&format!("{BASE}/{id}")).token(token).send().await;
        res.assert_status(404);
        assert_eq!(res.json, not_found);
    }
}

#[tokio::test]
async fn cancel_rules() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let w0 = workday(&app, 0).await;
    let cancel =
        |token: &str, id: &str| app.post(&format!("{BASE}/{id}/cancel")).token(token).send();

    // Pengaju membatalkan PENDING → CANCELLED BY_REQUESTER.
    let res = app
        .post(BASE)
        .token(&c.staff.token)
        .json(&json!({"date": w0, "durationMinutes": 60, "reason": "x"}))
        .send()
        .await;
    res.assert_status(201);
    let id = res.data()["id"].as_str().unwrap().to_owned();
    // staff2 (pihak tak terkait, bukan approver) → 404.
    let res = cancel(&c.staff2.token, &id).await;
    res.assert_status(404);
    assert_eq!(res.json["code"], "OVERTIME_NOT_FOUND");

    let res = cancel(&c.staff.token, &id).await;
    res.assert_status(200);
    let d = res.data();
    assert_eq!(d["status"], "CANCELLED");
    assert_eq!(d["cancelReason"], "BY_REQUESTER");
    assert_eq!(d["cancelReasonLabel"], "Dibatalkan pengaju");
    assert_eq!(d["dayType"], "WORKDAY");
    let (by, at, upd): (Option<i32>, bool, bool) = sqlx::query_as(
        r#"SELECT "cancelledById", "cancelledAt" IS NOT NULL, "updatedAt" = "cancelledAt"
           FROM "overtimeRequest" WHERE uuid = $1::uuid"#,
    )
    .bind(&id)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!((by, at, upd), (Some(c.staff.id), true, true));

    // Sudah CANCELLED → 409 + status.
    let res = cancel(&c.staff.token, &id).await;
    res.assert_status(409);
    assert_eq!(
        res.json,
        json!({"code": "OVERTIME_STATE_CONFLICT", "message": "Pengajuan lembur ini sudah tidak dapat dibatalkan.", "details": {"status": "CANCELLED"}})
    );
    // Baris CANCELLED lama tidak menghalangi pengajuan baru di tanggal yang sama.
    app.post(BASE)
        .token(&c.staff.token)
        .json(&json!({"date": w0, "durationMinutes": 60, "reason": "lagi"}))
        .send()
        .await
        .assert_status(201);

    // ORDER dari SUPER_ADMIN ke staff.
    let w1 = workday(&app, 1).await;
    let order = raw_request(
        &app,
        &c,
        c.staff.id,
        c.super_admin.id,
        "ORDER",
        "PENDING_EMPLOYEE",
        &w1,
        60,
        "now() + interval '1 day'",
    )
    .await;
    let not_requester = ot_error(
        "OVERTIME_NOT_REQUESTER",
        "Hanya pengaju yang dapat membatalkan pengajuan lembur ini.",
    );
    // Karyawan yang dituju (bukan pengaju) → 403.
    let res = cancel(&c.staff.token, &order).await;
    res.assert_status(403);
    assert_eq!(res.json, not_requester);
    // Approver lain (MANAGER) → 403; non-approver (ADMIN) → 404.
    let res = cancel(&c.manager.token, &order).await;
    res.assert_status(403);
    assert_eq!(res.json, not_requester);
    cancel(&c.admin.token, &order).await.assert_status(404);
    // Pemberi perintah → batal.
    let res = cancel(&c.super_admin.token, &order).await;
    res.assert_status(200);
    assert_eq!(res.data()["status"], "CANCELLED");

    // T121: lewat tenggat, masih PENDING → 409 OVERTIME_DEADLINE_PASSED.
    let past_date = jkt_date(&app, -3).await;
    let late = raw_request(
        &app,
        &c,
        c.staff.id,
        c.staff.id,
        "SELF",
        "PENDING_APPROVAL",
        &past_date,
        60,
        "now() - interval '1 minute'",
    )
    .await;
    let res = cancel(&c.staff.token, &late).await;
    res.assert_status(409);
    assert_eq!(
        res.json,
        ot_error(
            "OVERTIME_DEADLINE_PASSED",
            "Batas waktu pengajuan lembur ini sudah lewat dan tidak dapat dibatalkan lagi."
        )
    );
}

#[tokio::test]
async fn accept_and_reject_orders() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let tomorrow = "now() + interval '1 day'";
    let w0 = workday(&app, 0).await;
    let w1 = workday(&app, 1).await;
    let w2 = workday(&app, 2).await;
    let w3 = workday(&app, 3).await;
    let order = raw_request(
        &app,
        &c,
        c.staff.id,
        c.super_admin.id,
        "ORDER",
        "PENDING_EMPLOYEE",
        &w0,
        60,
        tomorrow,
    )
    .await;
    let act = |token: &str, id: &str, action: &str, body: Option<Value>| {
        let req = app.post(&format!("{BASE}/{id}/{action}")).token(token);
        let req = match body {
            Some(b) => req.json(&b),
            None => req,
        };
        req.send()
    };

    // Bukan ORDER untuk pemanggil → 404.
    act(&c.staff2.token, &order, "accept", None)
        .await
        .assert_status(404);
    // Body tidak valid → 400.
    let res = act(
        &c.staff.token,
        &order,
        "accept",
        Some(json!({"confirmOverQuota": null})),
    )
    .await;
    res.assert_status(400);
    assert_eq!(
        res.json,
        json!({"message": ["confirmOverQuota must be a boolean value"], "error": "Bad Request", "statusCode": 400})
    );

    // Terima tanpa body → APPROVED + snapshot, decidedBy = karyawan.
    let res = act(&c.staff.token, &order, "accept", None).await;
    res.assert_status(200);
    let d = res.data();
    assert_eq!(d["status"], "APPROVED");
    assert_eq!(d["decidedBy"]["id"], c.staff.uuid);
    assert_eq!(d["dayType"], "WORKDAY");
    assert_eq!(d["normalMinutes"], 480);
    assert_eq!(d["dailyLimitMinutes"], 240);
    assert_eq!(d["exceedsDailyLimit"], false);
    assert!(d["decidedAt"].is_string());
    let (pwd, approved, upd): (Option<i16>, bool, bool) = sqlx::query_as(
        r#"SELECT "patternWorkDays", "approvedAt" = "decidedAt", "updatedAt" = "decidedAt"
           FROM "overtimeRequest" WHERE uuid = $1::uuid"#,
    )
    .bind(&order)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!((pwd, approved, upd), (Some(5), true, true));
    // Sudah APPROVED → 409.
    let res = act(&c.staff.token, &order, "accept", None).await;
    res.assert_status(409);
    assert_eq!(
        res.json,
        ot_error(
            "OVERTIME_STATE_CONFLICT",
            "Status pengajuan lembur sudah berubah. Muat ulang lalu coba lagi."
        )
    );

    // Melebihi kuota harian: tanpa konfirmasi 409, dengan konfirmasi 200.
    let big = raw_request(
        &app,
        &c,
        c.staff.id,
        c.super_admin.id,
        "ORDER",
        "PENDING_EMPLOYEE",
        &w1,
        300,
        tomorrow,
    )
    .await;
    let res = act(&c.staff.token, &big, "accept", Some(json!({}))).await;
    res.assert_status(409);
    assert_eq!(res.json["code"], "OVERTIME_QUOTA_CONFIRMATION_REQUIRED");
    assert_eq!(res.json["details"]["exceedsDaily"], true);
    let res = act(
        &c.staff.token,
        &big,
        "accept",
        Some(json!({"confirmOverQuota": true})),
    )
    .await;
    res.assert_status(200);
    assert_eq!(res.data()["exceedsDailyLimit"], true);
    assert_eq!(res.data()["exceedsLimit"], true);

    // Tolak: alasan wajib (400), lalu sukses dengan alasan ter-trim.
    let rej = raw_request(
        &app,
        &c,
        c.staff.id,
        c.super_admin.id,
        "ORDER",
        "PENDING_EMPLOYEE",
        &w2,
        60,
        tomorrow,
    )
    .await;
    let res = act(&c.staff.token, &rej, "reject", None).await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "reason must be shorter than or equal to 500 characters",
            "reason should not be empty",
            "reason must be a string"
        ])
    );
    let res = act(
        &c.staff.token,
        &rej,
        "reject",
        Some(json!({"reason": "  Tidak bisa  "})),
    )
    .await;
    res.assert_status(200);
    let d = res.data();
    assert_eq!(d["status"], "REJECTED");
    assert_eq!(d["rejectReason"], "Tidak bisa");
    assert_eq!(d["decidedBy"]["id"], c.staff.uuid);
    assert_eq!(d["dayType"], Value::Null);
    let res = act(&c.staff.token, &rej, "reject", Some(json!({"reason": "x"}))).await;
    res.assert_status(409);

    // T10: lewat tenggat → 409 OVERTIME_DEADLINE_PASSED (accept & reject).
    let late = raw_request(
        &app,
        &c,
        c.staff.id,
        c.super_admin.id,
        "ORDER",
        "PENDING_EMPLOYEE",
        &w3,
        60,
        "now() - interval '1 minute'",
    )
    .await;
    for (action, body) in [("accept", None), ("reject", Some(json!({"reason": "x"})))] {
        let res = act(&c.staff.token, &late, action, body).await;
        res.assert_status(409);
        assert_eq!(
            res.json,
            ot_error(
                "OVERTIME_DEADLINE_PASSED",
                "Batas waktu menjawab perintah lembur ini sudah lewat."
            )
        );
    }

    // SELF milik sendiri bukan ORDER → 404.
    let w4 = workday(&app, 4).await;
    let own = raw_request(
        &app,
        &c,
        c.staff.id,
        c.staff.id,
        "SELF",
        "PENDING_APPROVAL",
        &w4,
        60,
        tomorrow,
    )
    .await;
    act(&c.staff.token, &own, "accept", None)
        .await
        .assert_status(404);
}

#[tokio::test]
async fn disabled_feature_blocks_everything() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let w0 = workday(&app, 0).await;
    let id = raw_request(
        &app,
        &c,
        c.staff.id,
        c.super_admin.id,
        "ORDER",
        "PENDING_EMPLOYEE",
        &w0,
        60,
        "now() + interval '1 day'",
    )
    .await;
    app.exec(&format!(
        r#"UPDATE "companyOvertimeSetting" SET enabled = false WHERE "companyId" = {}"#,
        c.id
    ))
    .await;
    let disabled = ot_error(
        "OVERTIME_DISABLED",
        "Fitur lembur sedang dinonaktifkan untuk perusahaan ini.",
    );
    let token = &c.staff.token;
    let cases = vec![
        app.get(&format!("{BASE}/me")).token(token),
        app.get(&format!("{BASE}/quota?date={w0}&durationMinutes=60"))
            .token(token),
        app.post(BASE)
            .token(token)
            .json(&json!({"date": w0, "durationMinutes": 60, "reason": "x"})),
        app.get(&format!("{BASE}/{id}")).token(token),
        app.post(&format!("{BASE}/{id}/cancel")).token(token),
        app.post(&format!("{BASE}/{id}/accept")).token(token),
        app.post(&format!("{BASE}/{id}/reject"))
            .token(token)
            .json(&json!({"reason": "x"})),
    ];
    for req in cases {
        let res = req.send().await;
        res.assert_status(403);
        assert_eq!(res.json, disabled, "{} {}", res.method, res.uri);
    }
    let status: String = app
        .scalar(&format!(
            r#"SELECT status::text FROM "overtimeRequest" WHERE uuid = '{id}'"#
        ))
        .await;
    assert_eq!(status, "PENDING_EMPLOYEE");
}
