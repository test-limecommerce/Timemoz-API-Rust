//! Uji integrasi antrean `leave-schedule` (Redis + Postgres nyata) dan sinkron kalender cuti
//! terhadap server Google tiruan. Antrean bersama → uji di binary ini berurutan (`SERIAL`).
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-leave --test it_leave_worker`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::Duration;

use chrono::{Datelike, Duration as ChronoDuration};
use common::*;
use mz_core::queue::{Outcome, claim, get};
use mz_int_common::mock::MockServer;
use mz_leave::jobs::{
    HolidaySyncPayload, LEAVE_SCHEDULE_QUEUE, calendar_approved_job_id, enqueue_calendar_sync,
    enqueue_holiday_sync, enqueue_policy_switch, enqueue_reminder, holiday_sync_job_id,
    leave_reminder_job_id,
};
use mz_leave::worker::{process, tick_company};
use mz_testkit::TestApp;
use serde_json::json;
use tokio::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::const_new(());

const JKT: &str = "Asia/Jakarta";

/// Kosongkan antrean (sisa run sebelumnya di Redis DB uji).
async fn drain(app: &TestApp) {
    loop {
        let jobs = claim(
            app.state(),
            LEAVE_SCHEDULE_QUEUE,
            100,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        if jobs.is_empty() {
            break;
        }
        for j in jobs {
            mz_core::queue::ack(app.state(), LEAVE_SCHEDULE_QUEUE, &j.id)
                .await
                .unwrap();
        }
    }
}

/// Klaim job jatuh tempo dengan id `id` dan jalankan prosesnya.
async fn run_job(app: &TestApp, id: &str) -> Outcome {
    let jobs = claim(
        app.state(),
        LEAVE_SCHEDULE_QUEUE,
        100,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    let job = jobs
        .into_iter()
        .find(|j| j.id == id)
        .unwrap_or_else(|| panic!("job {id} tidak jatuh tempo"));
    let out = process(app.state().clone(), job).await;
    mz_core::queue::ack(app.state(), LEAVE_SCHEDULE_QUEUE, id)
        .await
        .unwrap();
    out
}

/// Hari kerja (Sen–Jum) paling akhir yang ≤ hari ini (WIB) di tahun berjalan.
fn recent_weekday() -> Option<chrono::NaiveDate> {
    let today = mz_leave::policy::local_now(mz_core::time::now_naive(), JKT).date();
    (0..7)
        .map(|i| today - ChronoDuration::days(i))
        .find(|d| d.weekday().number_from_monday() <= 5 && d.year() == today.year())
}

#[tokio::test]
async fn enqueue_dedupes_and_delays() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    drain(&app).await;
    let p = HolidaySyncPayload {
        company_id: 1,
        holiday_id: 99,
        previous_date: None,
        current_date: Some("2026-10-01".into()),
    };
    enqueue_holiday_sync(app.state(), &p, 123).await;
    enqueue_holiday_sync(app.state(), &p, 123).await;
    let id = holiday_sync_job_id(99, 123);
    let stored = get(app.state(), LEAVE_SCHEDULE_QUEUE, &id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.data,
        json!({"name": "leave.holiday-sync", "data": {"companyId": 1, "holidayId": 99, "currentDate": "2026-10-01"}})
    );
    assert_eq!((stored.max_attempts, stored.backoff_ms), (3, 5_000));
    let due = claim(
        app.state(),
        LEAVE_SCHEDULE_QUEUE,
        100,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(due.iter().filter(|j| j.id == id).count(), 1);
    drain(&app).await;

    // Pengingat: tersimpan tapi baru jatuh tempo 48 jam lagi.
    enqueue_reminder(app.state(), 5, 777).await;
    let rid = leave_reminder_job_id(5, 777);
    let stored = get(app.state(), LEAVE_SCHEDULE_QUEUE, &rid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((stored.max_attempts, stored.backoff_ms), (3, 60_000));
    let due = claim(
        app.state(),
        LEAVE_SCHEDULE_QUEUE,
        100,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert!(due.iter().all(|j| j.id != rid));
    mz_core::queue::ack(app.state(), LEAVE_SCHEDULE_QUEUE, &rid)
        .await
        .unwrap();

    enqueue_calendar_sync(app.state(), 7, calendar_approved_job_id(7)).await;
    let stored = get(app.state(), LEAVE_SCHEDULE_QUEUE, "leave-cal:7:approved")
        .await
        .unwrap()
        .unwrap();
    assert_eq!((stored.max_attempts, stored.backoff_ms), (5, 30_000));
    drain(&app).await;
}

#[tokio::test]
async fn invalid_payloads_are_dropped() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let st = app.state();
    let job = |data| mz_core::queue::Job {
        id: "x".into(),
        data,
        attempt: 1,
        max_attempts: 3,
        backoff_ms: 0,
        enqueued_at: 0,
    };
    for data in [
        json!({"name": "leave.holiday-sync", "data": {"companyId": 1}}),
        json!({"name": "leave.holiday-sync", "data": {"companyId": 1, "holidayId": 2, "previousDate": "2026-1-1"}}),
        json!({"name": "leave.policy-switch", "data": {"companyId": "1"}}),
        json!({"name": "leave.calendar-sync", "data": {"leaveRequestId": 0}}),
        json!({"name": "leave.reminder", "data": {}}),
        json!({"name": "leave.unknown", "data": {}}),
    ] {
        assert!(matches!(
            process(st.clone(), job(data)).await,
            Outcome::Drop(_)
        ));
    }
    // Pengajuan tidak ada → selesai tanpa galat.
    let out = process(st.clone(), job(json!({"name": "leave.reminder", "data": {"leaveRequestId": 2147483000, "updatedAtMs": 1}}))).await;
    assert_eq!(out, Outcome::Done);
}

#[tokio::test]
async fn holiday_sync_job_reconciles_and_recalculates() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let Some(day) = recent_weekday() else { return };
    drain(&app).await;
    let key = mz_leave::day_count::to_date_key(day);
    let co = company(&app, JKT, 1, "2024-01-01T00:00:00").await;
    let year = day.year();
    let b = balance(&app, co.id, co.user(), year, 12.0, 1.0, 0.0, 0.0).await;
    let h = holiday(&app, co.id, "Cuti Bersama", &key, true).await;
    let t = leave_type(&app, co.id, true, "ANNUAL").await;
    let r = request(
        &app,
        co.id,
        co.user(),
        Some(t),
        &key,
        &key,
        1.0,
        "APPROVED",
        None,
    )
    .await;

    let p = HolidaySyncPayload {
        company_id: co.id,
        holiday_id: h,
        previous_date: None,
        current_date: Some(key.clone()),
    };
    enqueue_holiday_sync(app.state(), &p, 1).await;
    assert_eq!(
        run_job(&app, &holiday_sync_job_id(h, 1)).await,
        Outcome::Done
    );
    // Potongan cuti bersama diterapkan (+1), cuti tersetujui di tanggal itu dihitung ulang
    // menjadi 0 hari (−1).
    let rows: i64 = app.scalar(&format!(r#"SELECT count(*) FROM "holidayLeaveDeduction" WHERE "userId" = {} AND status = 'APPLIED'"#, co.user())).await;
    assert_eq!(rows, 1);
    assert_eq!(
        f(
            &app,
            &format!(r#"SELECT "totalDays" FROM "leaveRequest" WHERE id = {r}"#)
        )
        .await,
        0.0
    );
    assert_eq!(
        f(
            &app,
            &format!(r#"SELECT "annualLeaveUsed" FROM "leaveBalance" WHERE id = {b}"#)
        )
        .await,
        1.0
    );
    let sources: Vec<String> = sqlx::query_scalar(
        r#"SELECT source::text FROM "leaveBalanceAdjustment" WHERE "userId" = $1 ORDER BY id"#,
    )
    .bind(co.user())
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(sources, vec!["HOLIDAY_DEDUCTION", "LEAVE_RECALC"]);
}

#[tokio::test]
async fn policy_switch_job_and_tick() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    drain(&app).await;
    let co = company(&app, JKT, 2, "2024-01-01T00:00:00").await;
    policy(&app, co.id, "", "").await;
    let now = mz_core::time::now_naive();
    let year = mz_leave::policy::local_year(now, JKT);
    let b = balance(&app, co.id, co.users[0], year, 3.0, 0.0, 0.0, 0.0).await;
    enqueue_policy_switch(app.state(), co.id, 42).await;
    assert_eq!(
        run_job(&app, &format!("policy-switch:{}:42", co.id)).await,
        Outcome::Done
    );
    assert_eq!(
        f(
            &app,
            &format!(r#"SELECT "annualLeaveTotal" FROM "leaveBalance" WHERE id = {b}"#)
        )
        .await,
        12.0
    );

    // Tick company: membuka saldo user kedua (ensureYearOpen) tanpa galat; tanpa token Google
    // tidak ada job kalender.
    request(
        &app,
        co.id,
        co.users[0],
        None,
        &format!("{}-12-30", year),
        &format!("{}-12-31", year),
        2.0,
        "APPROVED",
        None,
    )
    .await;
    tick_company(app.state(), co.id, JKT, now).await.unwrap();
    assert!(
        mz_leave::balance::find_balance(app.db(), co.users[1], co.id, year)
            .await
            .unwrap()
            .is_some()
    );
    let due = claim(
        app.state(),
        LEAVE_SCHEDULE_QUEUE,
        100,
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert!(due.iter().all(|j| !j.id.starts_with("leave-cal:")));
    drain(&app).await;
}

#[tokio::test]
async fn reminder_only_for_unchanged_pending() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let co = company(&app, JKT, 2, "2024-01-01T00:00:00").await;
    let r = request(
        &app,
        co.id,
        co.users[0],
        None,
        "2026-12-01",
        "2026-12-01",
        1.0,
        "PENDING",
        None,
    )
    .await;
    app.exec(&format!(r#"UPDATE "leaveRequest" SET "approverId" = {}, "updatedAt" = '2026-11-01T00:00:00.123' WHERE id = {r}"#, co.users[1])).await;
    let ms = at("2026-11-01T00:00:00.123Z").and_utc().timestamp_millis();
    let job = |ms: i64| mz_core::queue::Job {
        id: "r".into(),
        data: json!({"name": "leave.reminder", "data": {"leaveRequestId": r, "updatedAtMs": ms}}),
        attempt: 1,
        max_attempts: 3,
        backoff_ms: 0,
        enqueued_at: 0,
    };
    // Tanpa MAILTRAP_TOKEN email tidak terkirim; jalurnya tetap selesai tanpa galat.
    assert_eq!(process(app.state().clone(), job(ms)).await, Outcome::Done);
    assert_eq!(
        process(app.state().clone(), job(ms + 1)).await,
        Outcome::Done
    );
}

// ── Sinkron kalender (server Google tiruan) ──────────────────────────────────

async fn google_company(app: &TestApp) -> Co {
    let co = company(app, JKT, 2, "2024-01-01T00:00:00").await;
    app.exec(&format!(
        r#"UPDATE company SET "googleCalendarAccessToken" = 'acc-1', "googleCalendarRefreshToken" = 'ref-1',
             "googleCalendarTokenExpiresAt" = now() + interval '1 day' WHERE id = {}"#,
        co.id
    ))
    .await;
    co
}

#[tokio::test]
async fn calendar_sync_creates_and_removes_event() {
    let _g = SERIAL.lock().await;
    let mock = MockServer::start().await;
    let base = mock.url();
    let Some(app) = TestApp::with_vars(
        no_routes,
        &[
            ("MZ_GOOGLE_APIS_BASE", &base),
            ("MZ_GOOGLE_OAUTH_BASE", &base),
        ],
    )
    .await
    else {
        return;
    };
    drain(&app).await;
    mock.on(
        "POST",
        "/calendar/v3/calendars/primary/events",
        200,
        json!({"id": "evt-1", "htmlLink": "https://cal/evt-1"}),
    );
    mock.on(
        "DELETE",
        "/calendar/v3/calendars/primary/events/*",
        204,
        serde_json::Value::Null,
    );
    let co = google_company(&app).await;
    let r = request(
        &app,
        co.id,
        co.users[0],
        None,
        "2026-10-05",
        "2026-10-07",
        3.0,
        "APPROVED",
        None,
    )
    .await;
    app.exec(&format!(
        r#"UPDATE "leaveRequest" SET "reviewedById" = {} WHERE id = {r}"#,
        co.users[1]
    ))
    .await;
    let uuid: String = app
        .scalar(&format!(
            r#"SELECT uuid::text FROM "leaveRequest" WHERE id = {r}"#
        ))
        .await;

    enqueue_calendar_sync(app.state(), r, calendar_approved_job_id(r)).await;
    assert_eq!(
        run_job(&app, &calendar_approved_job_id(r)).await,
        Outcome::Done
    );
    let posts = mock.requests_to("POST", "/calendar/v3/calendars/primary/events");
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].query, "sendUpdates=none");
    assert_eq!(posts[0].header("authorization"), Some("Bearer acc-1"));
    let body = posts[0].json();
    assert_eq!(body["summary"], "Cuti — User 0");
    assert_eq!(body["start"], json!({"date": "2026-10-05"}));
    assert_eq!(body["end"], json!({"date": "2026-10-08"}));
    assert_eq!(
        body["extendedProperties"]["private"]["mudahLeaveRequestUuid"],
        uuid.as_str()
    );
    let row: (String, bool, String, Option<i32>, Option<i32>, String) = sqlx::query_as(
        r#"SELECT "googleEventId", "isAllDay", title, "userId", "createdByUserId", "htmlLink"
           FROM "googleCalendarEvent" WHERE "leaveRequestId" = $1"#,
    )
    .bind(r)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            "evt-1".into(),
            true,
            "Cuti — User 0".into(),
            None,
            Some(co.users[1]),
            "https://cal/evt-1".into()
        )
    );

    // Ulang: sudah punya event → tidak POST lagi.
    mz_leave::calendar_sync::ensure_leave_event(app.state(), r)
        .await
        .unwrap();
    assert_eq!(
        mock.requests_to("POST", "/calendar/v3/calendars/primary/events")
            .len(),
        1
    );

    // Tidak lagi APPROVED → event dihapus di Google dan barisnya dihapus.
    app.exec(&format!(
        r#"UPDATE "leaveRequest" SET status = 'REJECTED' WHERE id = {r}"#
    ))
    .await;
    mz_leave::calendar_sync::ensure_leave_event(app.state(), r)
        .await
        .unwrap();
    let dels = mock.requests_to("DELETE", "/calendar/v3/calendars/primary/events/evt-1");
    assert_eq!(
        (dels.len(), dels[0].query.as_str()),
        (1, "sendUpdates=none")
    );
    let left: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "googleCalendarEvent" WHERE "leaveRequestId" = {r}"#
        ))
        .await;
    assert_eq!(left, 0);
}

#[tokio::test]
async fn calendar_sync_errors_and_reenqueue() {
    let _g = SERIAL.lock().await;
    let mock = MockServer::start().await;
    let base = mock.url();
    let Some(app) = TestApp::with_vars(
        no_routes,
        &[
            ("MZ_GOOGLE_APIS_BASE", &base),
            ("MZ_GOOGLE_OAUTH_BASE", &base),
        ],
    )
    .await
    else {
        return;
    };
    drain(&app).await;
    let co = google_company(&app).await;
    let r = request(
        &app,
        co.id,
        co.users[0],
        None,
        "2026-10-05",
        "2026-10-05",
        0.5,
        "APPROVED",
        None,
    )
    .await;
    app.exec(&format!(
        r#"UPDATE "leaveRequest" SET "startDayType" = 'HALF_DAY_MORNING' WHERE id = {r}"#
    ))
    .await;

    // 403 → selesai tanpa baris (dicoba lagi oleh tick).
    mock.on(
        "POST",
        "/calendar/v3/calendars/primary/events",
        403,
        json!({"error": "forbidden"}),
    );
    mz_leave::calendar_sync::ensure_leave_event(app.state(), r)
        .await
        .unwrap();
    // 500 → galat (antrean mencoba ulang).
    mock.on(
        "POST",
        "/calendar/v3/calendars/primary/events",
        500,
        json!({}),
    );
    assert!(
        mz_leave::calendar_sync::ensure_leave_event(app.state(), r)
            .await
            .is_err()
    );
    let job = mz_core::queue::Job {
        id: "c".into(),
        data: json!({"name": "leave.calendar-sync", "data": {"leaveRequestId": r}}),
        attempt: 1,
        max_attempts: 5,
        backoff_ms: 0,
        enqueued_at: 0,
    };
    assert!(matches!(
        process(app.state().clone(), job).await,
        Outcome::Retry(_)
    ));
    // Setengah hari tunggal → event berjam di zona company.
    mock.on(
        "POST",
        "/calendar/v3/calendars/primary/events",
        200,
        json!({"id": "evt-h"}),
    );
    mz_leave::calendar_sync::ensure_leave_event(app.state(), r)
        .await
        .unwrap();
    let last = mock
        .requests_to("POST", "/calendar/v3/calendars/primary/events")
        .pop()
        .unwrap()
        .json();
    assert_eq!(last["summary"], "Cuti — User 0 (½ hari pagi)");
    assert_eq!(
        last["start"],
        json!({"dateTime": "2026-10-05T09:00:00", "timeZone": JKT})
    );
    let (all_day, start): (bool, chrono::NaiveDateTime) = sqlx::query_as(
        r#"SELECT "isAllDay", "startAt" FROM "googleCalendarEvent" WHERE "leaveRequestId" = $1"#,
    )
    .bind(r)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!((all_day, start), (false, at("2026-10-05T02:00:00Z")));

    // reenqueueMissing: cuti APPROVED yang belum berakhir & belum punya event.
    let now = mz_core::time::now_naive();
    let year = mz_leave::policy::local_year(now, JKT);
    let future = request(
        &app,
        co.id,
        co.users[1],
        None,
        &format!("{}-12-30", year + 1),
        &format!("{}-12-31", year + 1),
        2.0,
        "APPROVED",
        None,
    )
    .await;
    mz_leave::calendar_sync::reenqueue_missing(app.state(), co.id, JKT, now)
        .await
        .unwrap();
    let id = mz_leave::jobs::calendar_tick_job_id(future, now);
    assert!(
        get(app.state(), LEAVE_SCHEDULE_QUEUE, &id)
            .await
            .unwrap()
            .is_some()
    );
    drain(&app).await;
}
