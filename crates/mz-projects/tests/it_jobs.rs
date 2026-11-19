//! Uji integrasi job budget-daily-reset terhadap Postgres nyata.
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-projects --test it_jobs`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use mz_core::AppState;
use mz_projects::reset::{reset_budget, reset_cycle};
use mz_testkit::TestApp;

fn no_routes(_: &AppState) -> Router {
    Router::new()
}

/// Buat project + projectBudget langsung lewat SQL. Mengembalikan `projectId`.
async fn seed_budget(app: &TestApp, code: &str, enabled: bool, cycle: &str, hours: f64) -> i32 {
    app.exec(&format!(
        r#"INSERT INTO "projectType" (code, name, status) VALUES ('PT{code}', 'PT {code}', 1)
           ON CONFLICT DO NOTHING"#
    ))
    .await;
    let project_id: i32 = app
        .scalar(&format!(
            r#"WITH ins AS (
                 INSERT INTO project (code, name, "screenshotPerHour", "totalHour", "projectTypeId", "startDate", status, billable)
                 VALUES ('{code}', 'Proj {code}', 0, 0,
                   (SELECT id FROM "projectType" WHERE code = 'PT{code}'), '2026-01-01', 1, true)
                 RETURNING id)
               SELECT id FROM ins"#
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "projectBudget" ("projectId", "isEnabled", "resetCycle", "budgetHours",
             "currentBurnedSeconds", "thresholdAlertSentAt", "budgetReachedSentAt", "cycleStartAt")
           VALUES ({project_id}, {enabled}, '{cycle}'::"BudgetResetCycle", {hours}, 900,
             now(), now(), '2026-01-01')"#
    ))
    .await;
    project_id
}

async fn budget_state(app: &TestApp, project_id: i32) -> (i32, bool, bool) {
    let secs: i32 = app
        .scalar(&format!(
            r#"SELECT "currentBurnedSeconds" FROM "projectBudget" WHERE "projectId" = {project_id}"#
        ))
        .await;
    let alert_null: bool = app
        .scalar(&format!(
            r#"SELECT "thresholdAlertSentAt" IS NULL FROM "projectBudget" WHERE "projectId" = {project_id}"#
        ))
        .await;
    let last_reset_set: bool = app
        .scalar(&format!(
            r#"SELECT "lastResetAt" IS NOT NULL FROM "projectBudget" WHERE "projectId" = {project_id}"#
        ))
        .await;
    (secs, alert_null, last_reset_set)
}

#[tokio::test]
async fn weekly_reset_only_touches_enabled_weekly_budgets_with_hours() {
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let uniq = uuid::Uuid::new_v4().simple().to_string();
    let code = format!("RS{}", &uniq[..10]);
    let weekly = seed_budget(&app, &format!("{code}W"), true, "WEEKLY", 10.0).await;
    let monthly = seed_budget(&app, &format!("{code}M"), true, "MONTHLY", 10.0).await;
    let disabled = seed_budget(&app, &format!("{code}D"), false, "WEEKLY", 10.0).await;
    let zero = seed_budget(&app, &format!("{code}Z"), true, "WEEKLY", 0.0).await;

    reset_cycle(&app.state, "WEEKLY")
        .await
        .expect("reset weekly");

    assert_eq!(budget_state(&app, weekly).await, (0, true, true));
    assert_eq!(budget_state(&app, monthly).await, (900, false, false));
    assert_eq!(budget_state(&app, disabled).await, (900, false, false));
    assert_eq!(budget_state(&app, zero).await, (900, false, false));
}

#[tokio::test]
async fn reset_budget_zeroes_cost_and_sets_cycle_start() {
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let uniq = uuid::Uuid::new_v4().simple().to_string();
    let code = format!("RC{}", &uniq[..10]);
    let pid = seed_budget(&app, &code, true, "MONTHLY", 5.0).await;
    app.exec(&format!(
        r#"UPDATE "projectBudget" SET "currentBurnedCost" = 77 WHERE "projectId" = {pid}"#
    ))
    .await;
    let id: i32 = app
        .scalar(&format!(
            r#"SELECT id FROM "projectBudget" WHERE "projectId" = {pid}"#
        ))
        .await;
    reset_budget(&app.state, id).await.expect("reset one");
    let cost: f64 = app
        .scalar(&format!(
            r#"SELECT "currentBurnedCost" FROM "projectBudget" WHERE "projectId" = {pid}"#
        ))
        .await;
    let cycle_moved: bool = app
        .scalar(&format!(
            r#"SELECT "cycleStartAt" > '2026-01-01' FROM "projectBudget" WHERE "projectId" = {pid}"#
        ))
        .await;
    assert!(cost.abs() < f64::EPSILON);
    assert!(cycle_moved);
}

#[tokio::test]
async fn weekly_reset_includes_cost_budget_without_hours() {
    let Some(app) = TestApp::new(no_routes).await else {
        return;
    };
    let uniq = uuid::Uuid::new_v4().simple().to_string();
    let code = format!("RK{}", &uniq[..10]);
    // Budget COST: batas biaya saja, budgetHours = 0.
    let cost_only = seed_budget(&app, &code, true, "WEEKLY", 0.0).await;
    app.exec(&format!(
        r#"UPDATE "projectBudget" SET "budgetType" = 'COST', "budgetAmount" = 500,
             "billableRate" = 100, "currentBurnedCost" = 25 WHERE "projectId" = {cost_only}"#
    ))
    .await;

    reset_cycle(&app.state, "WEEKLY")
        .await
        .expect("reset weekly");

    assert_eq!(budget_state(&app, cost_only).await, (0, true, true));
    let cost: f64 = app
        .scalar(&format!(
            r#"SELECT "currentBurnedCost" FROM "projectBudget" WHERE "projectId" = {cost_only}"#
        ))
        .await;
    assert!(cost.abs() < f64::EPSILON);
}
