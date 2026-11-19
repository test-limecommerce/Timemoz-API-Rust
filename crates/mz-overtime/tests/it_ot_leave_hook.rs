//! Uji integrasi `cancel_for_leave` (Node `OvertimeLeaveHook.cancelForLeave`) terhadap
//! Postgres nyata. Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-overtime --test it_ot_leave_hook`.
//! Nama binary unik (database test diberi nama dari nama binary).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use chrono::NaiveDate;
use mz_core::AppState;
use mz_overtime::calendar::Leave;
use mz_overtime::{cancel_for_leave, full_day_dates_of_leave};
use mz_testkit::{Company, TestApp};
use tokio::sync::Mutex;

/// Kedua test memakai company fixture & lock karyawan yang sama: dijalankan berurutan.
static SERIAL: Mutex<()> = Mutex::const_new(());

fn build(state: &AppState) -> Router {
    Router::new().with_state(state.clone())
}

fn d(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
}

async fn insert(app: &TestApp, c: &Company, user: i32, date: &str, kind: &str, status: &str) {
    let extra = if status == "COMPLETED" {
        r#", "overtimeSec""#
    } else {
        ""
    };
    let extra_val = if status == "COMPLETED" { ", 3600" } else { "" };
    app.exec(&format!(
        r#"INSERT INTO "overtimeRequest" ("companyId", "userId", "requestedById", kind, date, "requestedSec",
             reason, status, "retroWindowDays", "responseDeadline", "finalAt"{extra})
           VALUES ({c}, {user}, {user}, '{kind}', '{date}', 3600, 'uji', '{status}', 7,
             now() + interval '1 day', now() + interval '1 day'{extra_val})"#,
        c = c.id,
    ))
    .await;
}

async fn status_of(app: &TestApp, c: &Company, user: i32, date: &str) -> String {
    app.scalar(&format!(
        r#"SELECT string_agg(status::text || ':' || coalesce("cancelReason"::text, '-') || ':'
                  || coalesce("cancelledById"::text, '-') || ':' || ("cancelledAt" IS NOT NULL)::text, ',')
           FROM "overtimeRequest" WHERE "companyId" = {c} AND "userId" = {user} AND date = '{date}'"#,
        c = c.id,
    ))
    .await
}

#[tokio::test]
async fn cancels_unfinished_overtime_on_full_leave_days() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;
    app.exec(&format!(
        r#"DELETE FROM "overtimeRequest" WHERE "companyId" = {}"#,
        c.id
    ))
    .await;
    let s = c.staff.id;
    let s2 = c.staff2.id;
    // Cuti 2026-12-01 (siang) s/d 2026-12-04 (pagi): hari penuh = 02 & 03.
    insert(&app, &c, s, "2026-12-01", "SELF", "PENDING_APPROVAL").await;
    insert(&app, &c, s, "2026-12-02", "SELF", "PENDING_APPROVAL").await;
    insert(&app, &c, s, "2026-12-03", "ORDER", "PENDING_EMPLOYEE").await;
    insert(&app, &c, s, "2026-12-04", "SELF", "APPROVED").await;
    insert(&app, &c, s2, "2026-12-02", "SELF", "APPROVED").await;

    let dates = full_day_dates_of_leave(&Leave {
        start_date: d("2026-12-01"),
        end_date: d("2026-12-04"),
        start_day_type: "HALF_DAY_AFTERNOON".into(),
        end_day_type: "HALF_DAY_MORNING".into(),
    });
    assert_eq!(dates, vec![d("2026-12-02"), d("2026-12-03")]);

    let now = mz_core::time::now_naive();
    let mut tx = app.db().begin().await.unwrap();
    let n = cancel_for_leave(&mut tx, c.id, s, &dates, now)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(n, 2);

    // Hari penuh dibatalkan sistem: cancelledById NULL, alasan EMPLOYEE_ON_LEAVE.
    for date in ["2026-12-02", "2026-12-03"] {
        assert_eq!(
            status_of(&app, &c, s, date).await,
            "CANCELLED:EMPLOYEE_ON_LEAVE:-:true",
            "{date}"
        );
    }
    // Ujung setengah hari & user lain tidak tersentuh.
    assert_eq!(
        status_of(&app, &c, s, "2026-12-01").await,
        "PENDING_APPROVAL:-:-:false"
    );
    assert_eq!(
        status_of(&app, &c, s, "2026-12-04").await,
        "APPROVED:-:-:false"
    );
    assert_eq!(
        status_of(&app, &c, s2, "2026-12-02").await,
        "APPROVED:-:-:false"
    );

    // COMPLETED tidak disentuh; daftar tanggal kosong = no-op tanpa lock.
    insert(&app, &c, s2, "2026-12-03", "SELF", "COMPLETED").await;
    let mut tx = app.db().begin().await.unwrap();
    let n = cancel_for_leave(&mut tx, c.id, s2, &[d("2026-12-03")], now)
        .await
        .unwrap();
    assert_eq!(n, 0);
    assert_eq!(
        cancel_for_leave(&mut tx, c.id, s2, &[], now).await.unwrap(),
        0
    );
    tx.commit().await.unwrap();
    assert_eq!(
        status_of(&app, &c, s2, "2026-12-03").await,
        "COMPLETED:-:-:false"
    );
}

/// Lock `ot-emp:` dipegang sampai transaksi cuti selesai (pengajuan lembur yang berjalan
/// bersamaan menunggu): `pg_try_advisory_xact_lock` dari koneksi lain gagal selama itu.
#[tokio::test]
async fn holds_employee_lock_until_commit() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;
    let now = mz_core::time::now_naive();
    let mut tx = app.db().begin().await.unwrap();
    cancel_for_leave(&mut tx, c.id, c.staff.id, &[d("2026-12-10")], now)
        .await
        .unwrap();
    let probe = format!(
        "SELECT pg_try_advisory_xact_lock(hashtext('ot-emp:' || {}::text || ':' || {}::text))",
        c.id, c.staff.id
    );
    let taken: bool = app.scalar(&probe).await;
    assert!(!taken, "lock karyawan harus masih dipegang transaksi cuti");
    tx.commit().await.unwrap();
    let taken: bool = app.scalar(&probe).await;
    assert!(taken, "lock harus lepas sesudah commit");
}
