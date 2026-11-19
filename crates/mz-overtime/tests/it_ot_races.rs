//! Uji balapan (request bersamaan) dan penjaga cuti pengajuan lembur lewat HTTP (router lembur
//! lengkap) terhadap Postgres & Redis nyata:
//! - dua approver memutuskan pengajuan yang sama bersamaan → tepat satu menang, yang lain
//!   `409 OVERTIME_STATE_CONFLICT` (CAS `WHERE status = 'PENDING_APPROVAL'`);
//! - dua pengajuan bersamaan untuk karyawan + tanggal yang sama → satu `201`, yang lain
//!   `409 OVERTIME_DATE_TAKEN` (lock karyawan + partial unique index), tanpa baris ganda;
//! - `422 OVERTIME_EMPLOYEE_ON_LEAVE` pada cuti sehari penuh; cuti setengah hari memotong jam
//!   normal (`OVERTIME_DURATION_EXCEEDS_DAY` memakai jam normal sesudah cuti).
//!
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-overtime --test it_ot_races`.

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

/// Putaran per balapan.
const ROUNDS: i32 = 6;

const STATE_CONFLICT: &str = "Status pengajuan lembur sudah berubah. Muat ulang lalu coba lagi.";

fn build(state: &AppState) -> Router {
    mz_overtime::hr::router(state)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

/// Fitur aktif (jendela 7), approver ADMIN + MANAGER, tanpa pengajuan & tanpa cuti disetujui
/// yang mengganggu; zona Asia/Jakarta.
async fn setup() -> Option<(TestApp, Company)> {
    let app = TestApp::new(build).await?;
    let c = app.company().await;
    app.exec(
        r#"INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
           SELECT al.id, f.k, f.a::"PermissionAction" FROM "accessLevel" al
           CROSS JOIN (VALUES ('overtime', 'READ'), ('overtime', 'WRITE')) AS f(k, a)
           WHERE al."isSystem" AND al.code IN ('SYSTEM_ADMIN', 'SUPER_ADMIN', 'ADMIN', 'MANAGER', 'STAFF')
           ON CONFLICT DO NOTHING"#,
    )
    .await;
    app.exec(&format!(
        r#"DELETE FROM "overtimeRequest" WHERE "companyId" = {c};
           DELETE FROM "overtimeApprover" WHERE "companyId" = {c};
           DELETE FROM "companyOvertimeSetting" WHERE "companyId" = {c};
           INSERT INTO "companyOvertimeSetting" ("companyId", enabled, "retroWindowDays", "createdAt", "updatedAt")
             VALUES ({c}, true, 7, now(), now());
           INSERT INTO "overtimeApprover" ("companyId", "userId") VALUES ({c}, {a}), ({c}, {m});
           UPDATE "leaveRequest" SET status = 'REJECTED'
             WHERE "companyId" = {c} AND status = 'APPROVED' AND "endDate" >= current_date - 30;
           UPDATE company SET "timeZone" = 'Asia/Jakarta' WHERE id = {c};"#,
        c = c.id,
        a = c.admin.id,
        m = c.manager.id,
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

/// `overtimeRequest` SELF `PENDING_APPROVAL` mentah (1 jam, tenggat jauh); mengembalikan uuid.
async fn pending(app: &TestApp, c: &Company, user: i32, date: &str) -> String {
    app.scalar(&format!(
        r#"WITH ins AS (
             INSERT INTO "overtimeRequest" ("companyId", "userId", "requestedById", kind, date, "requestedSec",
               reason, status, "retroWindowDays", "responseDeadline", "finalAt")
             VALUES ({c}, {user}, {user}, 'SELF', '{date}', 3600, 'uji', 'PENDING_APPROVAL', 7,
                     '2099-01-01T00:00:00', '2099-01-01T00:00:00')
             RETURNING uuid::text)
           SELECT uuid FROM ins"#,
        c = c.id
    ))
    .await
}

/// `(status, decidedById, approvedAt terisi, rejectReason)` baris pengajuan.
async fn decision(app: &TestApp, uuid: &str) -> (String, Option<i32>, bool, Option<String>) {
    sqlx::query_as(
        r#"SELECT status::text, "decidedById", "approvedAt" IS NOT NULL, "rejectReason"
           FROM "overtimeRequest" WHERE uuid = $1::uuid"#,
    )
    .bind(uuid)
    .fetch_one(app.db())
    .await
    .unwrap()
}

fn state_conflict() -> Value {
    json!({"code": "OVERTIME_STATE_CONFLICT", "message": STATE_CONFLICT})
}

#[tokio::test]
async fn concurrent_approvals_have_exactly_one_winner() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    for round in 0..ROUNDS {
        let uuid = pending(&app, &c, c.staff.id, &jkt_date(&app, round + 1).await).await;
        let path = format!("/v1/overtime/approvals/{uuid}/approve");
        let body = json!({"confirmOverQuota": true});
        let (a, m) = tokio::join!(
            app.post(&path).token(&c.admin.token).json(&body).send(),
            app.post(&path).token(&c.manager.token).json(&body).send(),
        );
        let statuses = [a.status.as_u16(), m.status.as_u16()];
        assert!(
            statuses == [200, 409] || statuses == [409, 200],
            "putaran {round}: {statuses:?} {} / {}",
            a.text(),
            m.text()
        );
        let (winner, winner_id, loser) = if statuses[0] == 200 {
            (&a, c.admin.id, &m)
        } else {
            (&m, c.manager.id, &a)
        };
        assert_eq!(loser.json, state_conflict(), "putaran {round}");
        assert_eq!(winner.data()["status"], "APPROVED");
        // Baris mencerminkan pemenang saja (tidak tertimpa yang kalah).
        let (status, decided_by, approved_at, reject_reason) = decision(&app, &uuid).await;
        assert_eq!(status, "APPROVED");
        assert_eq!(decided_by, Some(winner_id));
        assert!(approved_at);
        assert_eq!(reject_reason, None);
    }
}

#[tokio::test]
async fn concurrent_approve_and_reject_keep_one_decision() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let mut seen = [false, false];
    for round in 0..ROUNDS {
        let uuid = pending(&app, &c, c.staff2.id, &jkt_date(&app, round + 1).await).await;
        let (a, m) = tokio::join!(
            app.post(&format!("/v1/overtime/approvals/{uuid}/approve"))
                .token(&c.admin.token)
                .json(&json!({"confirmOverQuota": true}))
                .send(),
            app.post(&format!("/v1/overtime/approvals/{uuid}/reject"))
                .token(&c.manager.token)
                .json(&json!({"reason": "bentrok"}))
                .send(),
        );
        let statuses = [a.status.as_u16(), m.status.as_u16()];
        let (status, decided_by, approved_at, reject_reason) = decision(&app, &uuid).await;
        match statuses {
            [200, 409] => {
                seen[0] = true;
                assert_eq!(m.json, state_conflict(), "putaran {round}");
                assert_eq!(
                    (status.as_str(), decided_by, approved_at, reject_reason),
                    ("APPROVED", Some(c.admin.id), true, None)
                );
            }
            [409, 200] => {
                seen[1] = true;
                assert_eq!(a.json, state_conflict(), "putaran {round}");
                assert_eq!(m.data()["status"], "REJECTED");
                assert_eq!(
                    (status.as_str(), decided_by, approved_at, reject_reason),
                    (
                        "REJECTED",
                        Some(c.manager.id),
                        false,
                        Some("bentrok".to_owned())
                    )
                );
            }
            other => panic!("putaran {round}: {other:?} {} / {}", a.text(), m.text()),
        }
    }
    // Info saja: urutan pemenang bergantung penjadwalan.
    eprintln!("approve menang: {}, reject menang: {}", seen[0], seen[1]);
}

#[tokio::test]
async fn concurrent_submits_same_day_take_the_date_once() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    for round in 0..ROUNDS {
        let date = workday(&app, round).await;
        let body = |reason: &str| json!({"date": date, "durationMinutes": 30, "reason": reason, "confirmOverQuota": true});
        let (b1, b2) = (body("pertama"), body("kedua"));
        let (r1, r2) = tokio::join!(
            app.post("/v1/overtime/requests")
                .token(&c.staff.token)
                .json(&b1)
                .send(),
            app.post("/v1/overtime/requests")
                .token(&c.staff.token)
                .json(&b2)
                .send(),
        );
        let statuses = [r1.status.as_u16(), r2.status.as_u16()];
        assert!(
            statuses == [201, 409] || statuses == [409, 201],
            "putaran {round}: {statuses:?} {} / {}",
            r1.text(),
            r2.text()
        );
        let (winner, loser) = if statuses[0] == 201 {
            (&r1, &r2)
        } else {
            (&r2, &r1)
        };
        assert_eq!(
            loser.json,
            json!({"code": "OVERTIME_DATE_TAKEN", "message": "Sudah ada pengajuan lembur aktif di tanggal ini."}),
            "putaran {round}"
        );
        let n: i64 = app
            .scalar(&format!(
                r#"SELECT count(*) FROM "overtimeRequest"
                   WHERE "companyId" = {} AND "userId" = {} AND date = '{date}'"#,
                c.id, c.staff.id
            ))
            .await;
        assert_eq!(n, 1, "putaran {round}: baris ganda");
        let id: String = app
            .scalar(&format!(
                r#"SELECT uuid::text FROM "overtimeRequest"
                   WHERE "companyId" = {} AND "userId" = {} AND date = '{date}'"#,
                c.id, c.staff.id
            ))
            .await;
        assert_eq!(winner.data()["id"], id.as_str());
    }

    // Karyawan berbeda di tanggal yang sama tidak saling mengunci.
    let date = workday(&app, ROUNDS).await;
    let body =
        json!({"date": date, "durationMinutes": 30, "reason": "x", "confirmOverQuota": true});
    let (r1, r2) = tokio::join!(
        app.post("/v1/overtime/requests")
            .token(&c.staff.token)
            .json(&body)
            .send(),
        app.post("/v1/overtime/requests")
            .token(&c.staff2.token)
            .json(&body)
            .send(),
    );
    r1.assert_status(201);
    r2.assert_status(201);
}

#[tokio::test]
async fn submit_on_leave_day() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let full = workday(&app, 0).await;
    let half = workday(&app, 1).await;
    let rejected = workday(&app, 2).await;
    app.exec(&format!(
        r#"INSERT INTO "leaveRequest" ("userId", "companyId", "startDate", "endDate", "totalDays", reason, status)
             VALUES ({u}, {c}, '{full}', '{full}', 1, 'uji', 'APPROVED'),
                    ({u}, {c}, '{rejected}', '{rejected}', 1, 'uji', 'REJECTED');
           INSERT INTO "leaveRequest" ("userId", "companyId", "startDate", "endDate", "totalDays", reason, status,
                                       "startDayType", "endDayType")
             VALUES ({u}, {c}, '{half}', '{half}', 0.5, 'uji', 'APPROVED', 'HALF_DAY_MORNING', 'HALF_DAY_MORNING');"#,
        u = c.staff.id,
        c = c.id,
    ))
    .await;
    let post = |body: Value| {
        app.post("/v1/overtime/requests")
            .token(&c.staff.token)
            .json(&body)
            .send()
    };

    // Cuti sehari penuh disetujui → 422, tanpa baris.
    let res = post(json!({"date": full, "durationMinutes": 60, "reason": "x"})).await;
    res.assert_status(422);
    assert_eq!(
        res.json,
        json!({"code": "OVERTIME_EMPLOYEE_ON_LEAVE", "message": "Anda cuti sehari penuh di tanggal ini."})
    );
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "overtimeRequest" WHERE "companyId" = {} AND date = '{full}'"#,
            c.id
        ))
        .await;
    assert_eq!(n, 0);

    // Cuti setengah hari: jam normal 8 → 4 jam, jadi 20 jam lembur muat, 21 jam tidak.
    let res = post(json!({"date": half, "durationMinutes": 21 * 60, "reason": "x"})).await;
    res.assert_status(400);
    assert_eq!(
        res.json,
        json!({"code": "OVERTIME_DURATION_EXCEEDS_DAY", "message": "Lembur 21 jam + jam normal 4 jam melebihi 24 jam."})
    );
    let res = post(
        json!({"date": half, "durationMinutes": 20 * 60, "reason": "x", "confirmOverQuota": true}),
    )
    .await;
    res.assert_status(201);
    assert_eq!(res.data()["normalMinutes"], 240);

    // Cuti yang ditolak tidak menghalangi.
    post(json!({"date": rejected, "durationMinutes": 60, "reason": "x"}))
        .await
        .assert_status(201);
}
