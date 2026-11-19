//! Uji integrasi job manager summary & scheduled reports (Postgres nyata; Mailtrap tidak tersedia).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use chrono::{TimeZone, Utc};
use mz_core::AppState;
use mz_testkit::TestApp;
use mz_timesheets::jobs::{
    Collected, DispatchJob, Pacing, SummaryParams, claim_jobs, collect_daily_summary,
    collect_weekly_summary, dispatch, due_report_jobs, due_summary_jobs, handle_scheduled_report,
    handle_summary_daily,
};
use std::time::Duration;

fn build(_: &AppState) -> Router {
    Router::new()
}

/// Senin 2026-11-16 08:00 UTC.
fn monday() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 16, 8, 0, 0).unwrap()
}

fn params(ids: Vec<i32>) -> SummaryParams {
    SummaryParams {
        user_ids: Some(ids),
        low_hours_threshold: 14400,
        disabled_flags: vec![],
        include_weekends: false,
    }
}

fn zero_pacing() -> Pacing {
    Pacing {
        rest_min: Duration::ZERO,
        rest_max: Duration::ZERO,
        retry_delay: Duration::ZERO,
        job_timeout: Duration::from_secs(60),
    }
}

#[tokio::test]
async fn summary_collection_and_dispatch() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let co = app.company().await;
    app.exec(&format!(
        "UPDATE company SET \"timeZone\" = 'UTC' WHERE id = {}",
        co.id
    ))
    .await;
    let pid: i32 = app
        .scalar("SELECT id FROM project ORDER BY id LIMIT 1")
        .await;
    let (a, b) = (co.staff.id, co.staff2.id);
    // 2001-03-05 (Senin) .. 2001-03-11; minggu sebelumnya 2001-02-26 .. 03-04.
    app.exec(&format!(
        r#"INSERT INTO "dailyUserProjectSummary" ("reportDate","userId","projectId","companyId","totalSeconds") VALUES
           ('2001-03-05', {a}, {pid}, {c}, 20000), ('2001-03-06', {a}, {pid}, {c}, 8000),
           ('2001-03-05', {b}, {pid}, {c}, 3600)
           ON CONFLICT DO NOTHING;
           INSERT INTO "dailyCompanySummary" ("reportDate","companyId","totalSeconds") VALUES ('2001-02-26', {c}, 20000)
           ON CONFLICT DO NOTHING"#,
        c = co.id
    ))
    .await;

    // ── daily ──
    let Collected::Data(d) =
        collect_daily_summary(app.db(), co.id, "2001-03-05", &params(vec![a, b]))
            .await
            .unwrap()
    else {
        panic!("seharusnya ada anggota");
    };
    assert_eq!(d["totalHours"], "6h 33m");
    assert_eq!(d["activeMembers"], 2);
    assert_eq!(d["totalMembers"], 2);
    assert_eq!(d["teamRows"][0]["activity"], "green");
    assert_eq!(d["teamRows"][1]["activity"], "yellow");
    assert!(d["topProject"]["name"].is_string());
    assert!(
        d["flags"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("Low hours")
    );

    // Hari tanpa data: semua red + flag didn't track.
    let Collected::Data(d) =
        collect_daily_summary(app.db(), co.id, "2001-03-04", &params(vec![a, b]))
            .await
            .unwrap()
    else {
        panic!()
    };
    assert_eq!(d["noActivity"], true);
    assert!(
        d["flags"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("Didn't track")
    );

    // Tanpa anggota (userIds tak ada di company) -> NoMembers.
    assert_eq!(
        collect_daily_summary(app.db(), co.id, "2001-03-05", &params(vec![-1]))
            .await
            .unwrap(),
        Collected::NoMembers
    );

    // ── weekly ──
    let Collected::Data(w) = collect_weekly_summary(
        app.db(),
        co.id,
        "2001-03-05",
        "2001-03-11",
        &params(vec![a, b]),
    )
    .await
    .unwrap() else {
        panic!()
    };
    assert_eq!(w["totalHours"], "8h 46m"); // 31600 dtk
    assert_eq!(w["weekOverWeekChange"], "up 58% from last week");
    assert_eq!(w["startDate"], "2001-03-05");

    // ── pemilihan config jatuh tempo ──
    app.exec(&format!(
        r#"INSERT INTO "managerSummaryConfig" ("companyId","isDailyEnabled","isWeeklyEnabled","sendTime","includeWeekends","emails","userIds","createdById")
           VALUES ({c}, true, true, '08:00', false, '["ops@example.test"]', '[{a},{b}]', {u})
           ON CONFLICT ("companyId") DO UPDATE SET "isDailyEnabled"=true, "isWeeklyEnabled"=true, "sendTime"='08:00',
             "includeWeekends"=false, emails='["ops@example.test"]', "userIds"='[{a},{b}]',
             "lastDailySentAt"=NULL, "lastWeeklySentAt"=NULL"#,
        c = co.id,
        u = co.admin.id
    ))
    .await;
    let cfg_id: i32 = app
        .scalar(&format!(
            r#"SELECT id FROM "managerSummaryConfig" WHERE "companyId" = {}"#,
            co.id
        ))
        .await;
    let mark = format!(":{cfg_id}:");
    let mine = |jobs: &[DispatchJob]| -> Vec<String> {
        jobs.iter()
            .filter(|j| j.job_id().contains(&mark))
            .map(|j| j.job_id().to_owned())
            .collect()
    };
    let jobs = due_summary_jobs(app.db(), monday()).await.unwrap();
    assert_eq!(
        mine(&jobs),
        [
            format!("ms-daily:{cfg_id}:2026-11-16"),
            format!("ms-weekly:{cfg_id}:2026-11-16")
        ]
    );
    // Menit lain / Sabtu tidak jatuh tempo.
    let off = Utc.with_ymd_and_hms(2026, 11, 16, 8, 30, 0).unwrap();
    assert!(mine(&due_summary_jobs(app.db(), off).await.unwrap()).is_empty());
    let sat = Utc.with_ymd_and_hms(2026, 11, 21, 8, 0, 0).unwrap();
    assert!(mine(&due_summary_jobs(app.db(), sat).await.unwrap()).is_empty());

    // ── dedup claim_once (kunci setara jobId); butuh Redis test ──
    let job = DispatchJob::ScheduledReport {
        report_id: 0,
        start_date: "x".into(),
        end_date: "x".into(),
        job_id: format!("it-{}", uuid::Uuid::new_v4()),
    };
    assert_eq!(claim_jobs(&app.state, vec![job.clone()]).await.len(), 1);
    assert!(claim_jobs(&app.state, vec![job]).await.is_empty());

    // ── pengiriman: Mailtrap tidak ada -> gagal aman, lastDailySentAt tetap NULL ──
    handle_summary_daily(
        &app.state,
        cfg_id,
        "2001-03-05",
        "Mon, 5 Mar 2001",
        "Mon, 5 Mar 2001",
    )
    .await
    .unwrap();
    let sent: Option<String> = app
        .scalar(&format!(
            r#"SELECT "lastDailySentAt"::text FROM "managerSummaryConfig" WHERE id = {cfg_id}"#
        ))
        .await;
    assert!(
        sent.is_none(),
        "tanpa Mailtrap tidak boleh ditandai terkirim"
    );
    // Lewat dispatch() (pacing nol): config hilang = no-op, tidak panic.
    dispatch(
        &app.state,
        vec![
            DispatchJob::SummaryDaily {
                config_id: cfg_id,
                report_date: "2001-03-05".into(),
                report_date_formatted: "Mon, 5 Mar 2001".into(),
                subject_date: "Mon, 5 Mar 2001".into(),
                job_id: "t1".into(),
            },
            DispatchJob::SummaryDaily {
                config_id: -5,
                report_date: "2001-03-05".into(),
                report_date_formatted: String::new(),
                subject_date: String::new(),
                job_id: "t2".into(),
            },
        ],
        &zero_pacing(),
    )
    .await;
}

#[tokio::test]
async fn scheduled_report_selection_and_send() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let co = app.company().await;
    app.exec(&format!(
        "UPDATE company SET \"timeZone\" = 'UTC' WHERE id = {}",
        co.id
    ))
    .await;
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let ins = |name: &str, time: &str, freq: &str, active: bool, last: &str| {
        format!(
            r#"INSERT INTO "scheduledReport" (name, "sendTime", frequency, emails, subject, message, "isActive", "lastSentAt", "companyId", "createdById")
               VALUES ('{tag}-{name}', '{time}', '{freq}', '["x@example.test"]', 'Subj', 'Msg', {active}, {last}, {c}, {u}) RETURNING id"#,
            c = co.id,
            u = co.admin.id
        )
    };
    let id_of = |sql: String| {
        let app = &app;
        async move {
            let (id,): (i32,) = sqlx::query_as(&sql).fetch_one(app.db()).await.unwrap();
            id
        }
    };
    let due = id_of(ins("due", "08:00", "DAILY", true, "NULL")).await;
    let inactive = id_of(ins("inactive", "08:00", "DAILY", false, "NULL")).await;
    let other_time = id_of(ins("time", "09:00", "DAILY", true, "NULL")).await;
    let recent = id_of(ins(
        "recent",
        "08:00",
        "DAILY",
        true,
        "'2026-11-15 20:00:00'",
    ))
    .await; // 12 jam
    let monthly = id_of(ins("monthly", "08:00", "MONTHLY", true, "NULL")).await;
    let weekly_mon = id_of(ins("wk", "08:00", "WEEKLY", true, "NULL")).await;

    let jobs = due_report_jobs(app.db(), monday()).await.unwrap();
    let ids: Vec<&str> = jobs.iter().map(DispatchJob::job_id).collect();
    let has = |id: i32| ids.contains(&format!("scheduled-report:{id}:2026-11-16").as_str());
    assert!(has(due) && has(weekly_mon));
    assert!(!has(inactive) && !has(other_time) && !has(recent) && !has(monthly));
    let Some(DispatchJob::ScheduledReport {
        start_date,
        end_date,
        ..
    }) = jobs
        .iter()
        .find(|j| j.job_id().contains(&format!(":{due}:")))
    else {
        panic!()
    };
    assert_eq!(
        (start_date.as_str(), end_date.as_str()),
        ("2026-11-15", "2026-11-15")
    );

    // Tanggal 1 -> monthly jatuh tempo dengan rentang bulan lalu.
    let first = Utc.with_ymd_and_hms(2026, 12, 1, 8, 0, 0).unwrap();
    let jobs = due_report_jobs(app.db(), first).await.unwrap();
    let Some(DispatchJob::ScheduledReport {
        start_date,
        end_date,
        ..
    }) = jobs
        .iter()
        .find(|j| j.job_id().contains(&format!(":{monthly}:")))
    else {
        panic!("monthly harus jatuh tempo")
    };
    assert_eq!(
        (start_date.as_str(), end_date.as_str()),
        ("2026-11-01", "2026-11-30")
    );

    // Pengiriman tanpa Mailtrap: aman (tidak panic); Node tetap menandai lastSentAt
    // karena sendReport mengembalikan `{ success: true, failedCount }` tanpa melempar.
    handle_scheduled_report(&app.state, due, "2001-03-05", "2001-03-05")
        .await
        .unwrap();
    let sent: Option<String> = app
        .scalar(&format!(
            r#"SELECT "lastSentAt"::text FROM "scheduledReport" WHERE id = {due}"#
        ))
        .await;
    assert!(sent.is_some());
    // Report nonaktif -> no-op.
    handle_scheduled_report(&app.state, inactive, "2001-03-05", "2001-03-05")
        .await
        .unwrap();
    let sent: Option<String> = app
        .scalar(&format!(
            r#"SELECT "lastSentAt"::text FROM "scheduledReport" WHERE id = {inactive}"#
        ))
        .await;
    assert!(sent.is_none());
}
