//! Uji integrasi tick + recalc + nudge lembur terhadap Postgres & Redis nyata.
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-overtime --test it_overtime_jobs`.
//! (Nama binary sengaja bukan `it_jobs`: database test diberi nama dari nama binary, dan
//! `it_jobs` sudah dipakai crate lain → `mz_test_it_jobs` bisa di-drop di tengah jalan.)

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::Duration;

use axum::Router;
use chrono::{NaiveDateTime, Utc};
use mz_core::AppState;
use mz_overtime::nudge::NudgeInterval;
use mz_overtime::tick::{handle_recalc_at, handle_tick_at};
use mz_testkit::TestApp;
use tokio::sync::Mutex;

/// Tick memproses SEMUA company: test di binary ini dijalankan berurutan.
static SERIAL: Mutex<()> = Mutex::const_new(());

fn no_routes(_: &AppState) -> Router {
    Router::new()
}

fn ts(s: &str) -> NaiveDateTime {
    mz_core::time::parse_js_date(s).unwrap()
}

struct Fixture {
    company_id: i32,
    staff: i32,
    project_id: i32,
}

async fn fixture(app: &TestApp) -> Fixture {
    let c = app.company().await;
    // Zona company: Asia/Jakarta, awal minggu Senin.
    app.exec(&format!(
        r#"UPDATE company SET "timeZone" = 'Asia/Jakarta', "workweekStartDay" = 1 WHERE id = {}"#,
        c.id
    ))
    .await;
    let uniq = uuid::Uuid::new_v4().simple().to_string();
    let code = format!("OT{}", &uniq[..10]);
    app.exec(&format!(
        r#"INSERT INTO "projectType" (code, name, status) VALUES ('PT{code}', 'PT {code}', 1)"#
    ))
    .await;
    let project_id: i32 = app
        .scalar(&format!(
            r#"WITH ins AS (
                 INSERT INTO project (code, name, "screenshotPerHour", "totalHour", "projectTypeId", "startDate", status, billable)
                 VALUES ('{code}', 'Proj {code}', 0, 0,
                   (SELECT id FROM "projectType" WHERE code = 'PT{code}'), '2020-01-01', 1, true)
                 RETURNING id)
               SELECT id FROM ins"#
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "projectCompany" ("projectId", "companyId", status) VALUES ({project_id}, {}, 1)"#,
        c.id
    ))
    .await;
    Fixture {
        company_id: c.id,
        staff: c.staff.id,
        project_id,
    }
}

/// Timeline UTC (zona Asia/Jakarta) milik `user` di `[start, end)`.
async fn worked(app: &TestApp, f: &Fixture, taken: &str, start: &str, end: &str) {
    let tracking: i32 = app
        .scalar(&format!(
            r#"WITH ins AS (
                 INSERT INTO tracking ("takenDate", "userId", status, "isAbsent") VALUES ('{taken}', {}, 1, false)
                 ON CONFLICT ("userId", "takenDate") DO UPDATE SET status = 1
                 RETURNING id)
               SELECT id FROM ins"#,
            f.staff
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "trackingTimeline" (note, "isOffline", "startAt", "endAt", "startAtUtc", "endAtUtc",
             timezone, "trackingId", "projectId", status)
           VALUES ('ot', false, '{start}', '{end}', '{start}', '{end}', 'Asia/Jakarta', {tracking}, {}, 1)"#,
        f.project_id
    ))
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn request(
    app: &TestApp,
    f: &Fixture,
    date: &str,
    status: &str,
    requested_sec: i32,
    final_at: &str,
    deadline: &str,
) -> i32 {
    app.scalar(&format!(
        r#"WITH ins AS (
             INSERT INTO "overtimeRequest" ("companyId", "userId", "requestedById", kind, date, "requestedSec",
               reason, status, "retroWindowDays", "responseDeadline", "finalAt", "dayType", "baseNormalSec",
               "dailyLimitSec", "patternWorkDays", "approvedAt")
             VALUES ({c}, {u}, {u}, 'SELF', '{date}', {requested_sec}, 'uji', '{status}', 7, '{deadline}',
               '{final_at}', 'WORKDAY', 28800, 14400, 5, now())
             RETURNING id)
           SELECT id FROM ins"#,
        c = f.company_id,
        u = f.staff
    ))
    .await
}

#[derive(Debug, PartialEq, Eq)]
struct Figures {
    status: String,
    worked: Option<i32>,
    normal: Option<i32>,
    overtime: Option<i32>,
    finalized: bool,
    weekly: bool,
    limit: bool,
}

async fn figures(app: &TestApp, id: i32) -> Figures {
    let row: (
        String,
        Option<i32>,
        Option<i32>,
        Option<i32>,
        bool,
        bool,
        bool,
    ) = sqlx::query_as(
        r#"SELECT status::text, "workedSec", "normalSec", "overtimeSec", "finalizedAt" IS NOT NULL,
                  "exceedsWeeklyLimit", "exceedsLimit"
           FROM "overtimeRequest" WHERE id = $1"#,
    )
    .bind(id)
    .fetch_one(app.db())
    .await
    .unwrap();
    Figures {
        status: row.0,
        worked: row.1,
        normal: row.2,
        overtime: row.3,
        finalized: row.4,
        weekly: row.5,
        limit: row.6,
    }
}

fn fig(status: &str, w: i32, n: i32, o: i32, finalized: bool) -> Figures {
    Figures {
        status: status.into(),
        worked: Some(w),
        normal: Some(n),
        overtime: Some(o),
        finalized,
        weekly: false,
        limit: false,
    }
}

const NOW: &str = "2019-03-11T05:00:00Z"; // Senin 12:00 WIB

#[tokio::test]
async fn tick_completes_recalcs_cancels_and_finalizes() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let f = fixture(&app).await;
    let now = ts(NOW);

    // 2019-03-05 (Selasa): kerja 08:00–18:00 WIB = 10 jam.
    worked(
        &app,
        &f,
        "2019-03-05",
        "2019-03-05T01:00:00Z",
        "2019-03-05T11:00:00Z",
    )
    .await;
    let approved = request(
        &app,
        &f,
        "2019-03-05",
        "APPROVED",
        14_400,
        "2019-03-12T16:59:59.999Z",
        "2019-03-12T16:59:59.999Z",
    )
    .await;
    // Hari ini (lokal) belum berakhir → tetap APPROVED.
    let today = request(
        &app,
        &f,
        "2019-03-11",
        "APPROVED",
        3_600,
        "2019-03-18T16:59:59.999Z",
        "2019-03-18T16:59:59.999Z",
    )
    .await;
    // Pending lewat batas respons → CANCELLED NO_RESPONSE.
    let expired = request(
        &app,
        &f,
        "2019-03-02",
        "PENDING_APPROVAL",
        3_600,
        "2019-03-09T16:59:59.999Z",
        "2019-03-09T16:59:59.999Z",
    )
    .await;
    // APPROVED yang finalAt-nya sudah lewat: dihitung sekali lalu langsung final.
    worked(
        &app,
        &f,
        "2019-02-26",
        "2019-02-26T01:00:00Z",
        "2019-02-26T10:30:00Z",
    )
    .await;
    let late = request(
        &app,
        &f,
        "2019-02-26",
        "APPROVED",
        7_200,
        "2019-03-05T16:59:59.999Z",
        "2019-03-05T16:59:59.999Z",
    )
    .await;

    handle_tick_at(&app.state, now).await.unwrap();

    assert_eq!(
        figures(&app, approved).await,
        fig("COMPLETED", 36_000, 28_800, 7_200, false)
    );
    assert_eq!(figures(&app, today).await.status, "APPROVED");
    let (status, reason, cancelled): (String, Option<String>, bool) = sqlx::query_as(
        r#"SELECT status::text, "cancelReason"::text, "cancelledAt" IS NOT NULL FROM "overtimeRequest" WHERE id = $1"#,
    )
    .bind(expired)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), reason.as_deref(), cancelled),
        ("CANCELLED", Some("NO_RESPONSE"), true)
    );
    assert_eq!(
        figures(&app, late).await,
        fig("COMPLETED", 34_200, 28_800, 5_400, true)
    );

    // Cuti setengah hari APPROVED pada tanggal itu (dibaca LIVE) → recalc menurunkan jam normal.
    app.exec(&format!(
        r#"INSERT INTO "leaveRequest" ("userId", "companyId", "startDate", "endDate", "startDayType", "endDayType",
             "totalDays", reason, status)
           VALUES ({}, {}, '2019-03-05', '2019-03-05', 'HALF_DAY_MORNING', 'FULL_DAY', 0.5, 'uji', 'APPROVED')"#,
        f.staff, f.company_id
    ))
    .await;
    handle_recalc_at(
        &app.state,
        f.company_id,
        f.staff,
        &["2019-03-05".to_owned()],
        now,
    )
    .await
    .unwrap();
    assert_eq!(
        figures(&app, approved).await,
        fig("COMPLETED", 36_000, 14_400, 14_400, false)
    );

    // Payload tidak valid dibuang tanpa galat.
    handle_recalc_at(
        &app.state,
        f.company_id,
        f.staff,
        &["2025-02-30".to_owned()],
        now,
    )
    .await
    .unwrap();

    // Lewat finalAt → langkah 5 hanya mengisi finalizedAt; figur dibekukan.
    worked(
        &app,
        &f,
        "2019-03-05",
        "2019-03-05T12:00:00Z",
        "2019-03-05T13:00:00Z",
    )
    .await;
    handle_tick_at(&app.state, ts("2019-03-14T00:00:00Z"))
        .await
        .unwrap();
    assert_eq!(
        figures(&app, approved).await,
        fig("COMPLETED", 36_000, 14_400, 14_400, true)
    );
}

#[tokio::test]
async fn relabel_flags_week_over_18_hours() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let f = fixture(&app).await;
    // Minggu 2019-04-08 (Senin) … 2019-04-12: tiap hari kerja 12 jam (07:00–19:00 WIB) → 4 jam lembur.
    let mut ids = Vec::new();
    for day in 8..=12 {
        let date = format!("2019-04-{day:02}");
        worked(
            &app,
            &f,
            &date,
            &format!("{date}T00:00:00Z"),
            &format!("{date}T12:00:00Z"),
        )
        .await;
        ids.push(
            request(
                &app,
                &f,
                &date,
                "APPROVED",
                14_400,
                "2019-05-30T00:00:00Z",
                "2019-05-30T00:00:00Z",
            )
            .await,
        );
    }
    handle_tick_at(&app.state, ts("2019-04-15T05:00:00Z"))
        .await
        .unwrap();
    let mut flags = Vec::new();
    for id in &ids {
        let fgs = figures(&app, *id).await;
        assert_eq!(fgs.overtime, Some(14_400));
        flags.push((fgs.weekly, fgs.limit));
    }
    // 4+4+4+4 = 16 jam; hari kelima mendorong ke 20 jam > 18 jam.
    assert_eq!(
        flags,
        [
            (false, false),
            (false, false),
            (false, false),
            (false, false),
            (true, true)
        ]
    );
}

#[tokio::test]
async fn prune_removes_ineligible_approvers_with_system_audit() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let f = fixture(&app).await;
    let c = app.company().await;
    app.exec(&format!(
        r#"INSERT INTO "overtimeApprover" ("companyId", "userId") VALUES ({0}, {1}), ({0}, {2})
           ON CONFLICT DO NOTHING"#,
        f.company_id, f.staff, c.admin.id
    ))
    .await;
    // Tick hanya memangkas company yang punya baris belum final.
    request(
        &app,
        &f,
        "2019-06-03",
        "PENDING_APPROVAL",
        3_600,
        "2099-01-01T00:00:00Z",
        "2099-01-01T00:00:00Z",
    )
    .await;
    handle_tick_at(&app.state, ts("2019-06-04T00:00:00Z"))
        .await
        .unwrap();

    let remaining: Vec<i32> = sqlx::query_scalar(
        r#"SELECT "userId" FROM "overtimeApprover" WHERE "companyId" = $1 AND "userId" = ANY($2) ORDER BY "userId""#,
    )
    .bind(f.company_id)
    .bind(vec![f.staff, c.admin.id])
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(remaining, vec![c.admin.id]);
    let audited: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "auditLog" WHERE "companyId" = {} AND "subEntity" = 'overtimeApprover'
                 AND "actorType" = 'system' AND "actorName" = 'overtime-approver-prune'"#,
            f.company_id
        ))
        .await;
    assert!(audited >= 1, "audit prune tidak tercatat");
}

#[tokio::test]
async fn week_lock_is_shared_with_node_key() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let f = fixture(&app).await;
    worked(
        &app,
        &f,
        "2019-07-09",
        "2019-07-09T01:00:00Z",
        "2019-07-09T11:00:00Z",
    )
    .await;
    let id = request(
        &app,
        &f,
        "2019-07-09",
        "COMPLETED",
        3_600,
        "2099-01-01T00:00:00Z",
        "2099-01-01T00:00:00Z",
    )
    .await;

    // Kunci persis seperti `lockWeek` Node (Prisma mengikat angka lalu `::text`).
    let mut node = app.db().begin().await.unwrap();
    sqlx::query(&format!(
        r#"SELECT pg_advisory_xact_lock(hashtext('ot-week:' || {}::text || ':' || {}::text || ':' || '2019-07-08'::text))"#,
        f.company_id, f.staff
    ))
    .execute(&mut *node)
    .await
    .unwrap();

    let state = app.state.clone();
    let (c, u) = (f.company_id, f.staff);
    let job = tokio::spawn(async move {
        handle_recalc_at(
            &state,
            c,
            u,
            &["2019-07-09".to_owned()],
            mz_core::time::now_naive(),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        !job.is_finished(),
        "recalc tidak menunggu kunci minggu Node"
    );
    assert_eq!(figures(&app, id).await.worked, None);
    node.commit().await.unwrap();
    job.await.unwrap().unwrap();
    assert_eq!(figures(&app, id).await.worked, Some(36_000));
}

/// Redis DB test dipakai bersama antar run: hapus kunci dedup menit-menit terakhir.
async fn clear_claims(state: &AppState, company_id: i32, user_id: i32, date: &str) {
    use fred::prelude::KeysInterface;
    let now = Utc::now();
    for back in -1..=4 {
        let bucket = (now - chrono::Duration::minutes(back))
            .format("%Y%m%d%H%M")
            .to_string();
        let key = mz_overtime::nudge::recalc_claim_key(company_id, user_id, date, &bucket);
        let _: Result<(), _> = state.redis.next().del::<(), _>(key).await;
    }
}

async fn wait_for_worked(app: &TestApp, id: i32, expected: Option<i32>) -> bool {
    for _ in 0..50 {
        if figures(app, id).await.worked == expected {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test]
async fn nudge_recalcs_only_when_enabled() {
    let _g = SERIAL.lock().await;
    let Some(off) = TestApp::new(no_routes).await else {
        return;
    };
    let f = fixture(&off).await;
    worked(
        &off,
        &f,
        "2019-08-06",
        "2019-08-06T01:00:00Z",
        "2019-08-06T11:00:00Z",
    )
    .await;
    let id = request(
        &off,
        &f,
        "2019-08-06",
        "COMPLETED",
        7_200,
        "2099-01-01T00:00:00Z",
        "2099-01-01T00:00:00Z",
    )
    .await;
    let interval = NudgeInterval::new(
        ts("2019-08-06T01:00:00Z").and_utc().timestamp_millis(),
        ts("2019-08-06T11:00:00Z").and_utc().timestamp_millis(),
    );

    // MZ_JOBS kosong: tidak ada recalc dari jalur Rust (Node yang memegang antrean).
    mz_overtime::nudge_for_timelines(
        &off.state,
        Some(f.staff),
        vec![Some(f.project_id)],
        vec![interval],
    );
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(figures(&off, id).await.worked, None);

    let Some(on) = TestApp::with_vars(no_routes, &[("MZ_JOBS", "overtime-recalc")]).await else {
        return;
    };
    assert!(mz_overtime::recalc_enabled(&on.state));
    clear_claims(&on.state, f.company_id, f.staff, "2019-08-06").await;
    mz_overtime::nudge_for_timelines(
        &on.state,
        Some(f.staff),
        vec![Some(f.project_id)],
        vec![interval],
    );
    assert!(
        wait_for_worked(&on, id, Some(36_000)).await,
        "nudge tidak menghitung ulang"
    );
    assert_eq!(figures(&on, id).await.overtime, Some(7_200));

    // Sukses → kunci dedup menit itu dihapus (removeOnComplete), jadi nudge berikutnya
    // di menit yang sama tetap diproses.
    worked(
        &on,
        &f,
        "2019-08-06",
        "2019-08-06T12:00:00Z",
        "2019-08-06T13:00:00Z",
    )
    .await;
    mz_overtime::nudge(
        &on.state,
        f.company_id,
        f.staff,
        vec!["2019-08-06".into(), "bukan".into()],
    );
    assert!(
        wait_for_worked(&on, id, Some(39_600)).await,
        "nudge kedua tidak diproses"
    );

    // Kunci dedup yang masih dipegang (job sama sedang antre) → dilewati.
    let bucket = Utc::now().format("%Y%m%d%H%M").to_string();
    let key = mz_overtime::nudge::recalc_claim_key(f.company_id, f.staff, "2019-08-06", &bucket);
    assert!(
        mz_jobs::claim_once(&on.state, &key, Duration::from_secs(120))
            .await
            .unwrap()
    );
    worked(
        &on,
        &f,
        "2019-08-06",
        "2019-08-06T14:00:00Z",
        "2019-08-06T15:00:00Z",
    )
    .await;
    mz_overtime::nudge(&on.state, f.company_id, f.staff, vec!["2019-08-06".into()]);
    tokio::time::sleep(Duration::from_millis(700)).await;
    // Menit bisa berganti di tengah test; hanya periksa bila bucket masih sama.
    if Utc::now().format("%Y%m%d%H%M").to_string() == bucket {
        assert_eq!(figures(&on, id).await.worked, Some(39_600));
    }
    clear_claims(&on.state, f.company_id, f.staff, "2019-08-06").await;
}
