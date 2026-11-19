//! Bagian `src/project-budget/project-budget.service.ts` yang dipanggil route
//! projects: `findOne`, `upsert`, `recalculateBurn`, `checkAndFireAlerts`,
//! `getBudgetStatus`, dan kalkulasi ringkasan budget untuk daftar project.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime};
use mz_core::ApiResult;
use mz_core::AppState;
use mz_core::mail::{Mail, send};
use mz_core::time::{JsDate, now_naive};
use serde_json::{Value, json};

use crate::dto::BudgetInput;
use crate::jsfmt::{self, fixed, format_hours, locale, num, num_string, to_fixed};
use crate::rowjson;

/// Baris `projectBudget` (kolom skalar + dokumen JSON Prisma-shaped).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BudgetRow {
    pub id: i32,
    pub project_id: i32,
    pub is_enabled: bool,
    pub budget_type: String,
    pub billable_rate: Option<f64>,
    pub budget_amount: Option<f64>,
    pub count_non_billable: bool,
    pub current_burned_cost: f64,
    pub budget_hours: f64,
    pub reset_cycle: String,
    pub alert_threshold: i32,
    pub enforcement: String,
    pub cycle_start_at: Option<JsDate>,
    pub current_burned_seconds: i32,
    pub threshold_alert_sent_at: Option<JsDate>,
    pub budget_reached_sent_at: Option<JsDate>,
    pub doc: Value,
}

pub const BUDGET_SELECT: &str = r#"SELECT b.id, b."projectId" AS project_id, b."isEnabled" AS is_enabled,
    b."budgetType"::text AS budget_type, b."billableRate" AS billable_rate,
    b."budgetAmount" AS budget_amount, b."countNonBillable" AS count_non_billable,
    b."currentBurnedCost" AS current_burned_cost, b."budgetHours" AS budget_hours,
    b."resetCycle"::text AS reset_cycle, b."alertThreshold" AS alert_threshold,
    b.enforcement::text AS enforcement, b."cycleStartAt" AS cycle_start_at,
    b."currentBurnedSeconds" AS current_burned_seconds,
    b."thresholdAlertSentAt" AS threshold_alert_sent_at,
    b."budgetReachedSentAt" AS budget_reached_sent_at, to_jsonb(b) AS doc
    FROM "projectBudget" b"#;

pub async fn fetch_budget(state: &AppState, project_id: i32) -> sqlx::Result<Option<BudgetRow>> {
    sqlx::query_as::<_, BudgetRow>(&format!("{BUDGET_SELECT} WHERE b.\"projectId\" = $1"))
        .bind(project_id)
        .fetch_optional(&state.db)
        .await
}

/// `getBudgetStatus`.
/// Port `ProjectBudgetService.canStartTracking` bagian keputusan, setelah baris `projectBudget`
/// dibaca (`Mudah-API/src/project-budget/project-budget.service.ts:324-351`).
/// `role` = kode role pemilik (`?? 'STAFF'` oleh pemanggil). `Some(reason)` = `allowed: false`.
/// Nilai `None` diperlakukan 0 (baris budget tak ada / kolom null), sama seperti sebelum dipindah.
#[allow(clippy::too_many_arguments)]
pub fn can_start_tracking_block(
    role: &str,
    enabled: Option<bool>,
    budget_type: Option<&str>,
    budget_amount: Option<f64>,
    burned_cost: Option<f64>,
    budget_hours: Option<f64>,
    burned_seconds: Option<i32>,
    enforcement: Option<&str>,
) -> Option<&'static str> {
    if role != "STAFF" || enabled != Some(true) {
        return None;
    }
    let reached = if budget_type == Some("COST") {
        let amount = budget_amount.unwrap_or(0.0);
        if amount == 0.0 {
            return None;
        }
        burned_cost.unwrap_or(0.0) >= amount
    } else {
        let hours = budget_hours.unwrap_or(0.0);
        if hours == 0.0 {
            return None;
        }
        f64::from(burned_seconds.unwrap_or(0)) / 3600.0 >= hours
    };
    (reached && enforcement == Some("STOP_TRACKING"))
        .then_some("This project has reached its budget limit. Contact your manager.")
}

pub fn budget_status(percent_used: f64) -> &'static str {
    if percent_used >= 100.0 {
        "OVER_BUDGET"
    } else if percent_used >= 90.0 {
        "NEAR_LIMIT"
    } else {
        "ON_TRACK"
    }
}

/// Angka turunan budget bertipe COST.
#[derive(Debug, Clone, PartialEq)]
pub struct CostFigures {
    pub used_cost: f64,
    pub real_pct: f64,
    pub percent_used: f64,
    pub is_over: bool,
    pub over_amount: f64,
    pub over_pct: f64,
    pub remaining: f64,
}

pub fn cost_figures(burned_cost: f64, budget_amount: f64) -> CostFigures {
    let real_pct = if budget_amount > 0.0 {
        burned_cost / budget_amount * 100.0
    } else {
        0.0
    };
    let is_over = burned_cost > budget_amount;
    CostFigures {
        used_cost: fixed(burned_cost, 2),
        real_pct,
        percent_used: fixed(real_pct.min(100.0), 1),
        is_over,
        over_amount: if is_over {
            fixed(burned_cost - budget_amount, 2)
        } else {
            0.0
        },
        over_pct: if is_over && budget_amount > 0.0 {
            fixed((burned_cost - budget_amount) / budget_amount * 100.0, 1)
        } else {
            0.0
        },
        remaining: if is_over {
            0.0
        } else {
            fixed(budget_amount - burned_cost, 2)
        },
    }
}

/// Angka turunan budget bertipe HOURS.
#[derive(Debug, Clone, PartialEq)]
pub struct HourFigures {
    pub used_hours: f64,
    pub real_pct: f64,
    pub percent_used: f64,
    pub is_over: bool,
    pub over_hours: f64,
    pub over_pct: f64,
    pub remaining: f64,
}

pub fn hour_figures(burned_seconds: f64, budget_hours: f64) -> HourFigures {
    let burned = burned_seconds / 3600.0;
    let real_pct = if budget_hours > 0.0 {
        burned / budget_hours * 100.0
    } else {
        0.0
    };
    let is_over = burned > budget_hours;
    HourFigures {
        used_hours: fixed(burned, 2),
        real_pct,
        percent_used: fixed(real_pct.min(100.0), 1),
        is_over,
        over_hours: if is_over {
            fixed(burned - budget_hours, 2)
        } else {
            0.0
        },
        over_pct: if is_over && budget_hours > 0.0 {
            fixed((burned - budget_hours) / budget_hours * 100.0, 1)
        } else {
            0.0
        },
        remaining: if is_over {
            0.0
        } else {
            fixed(budget_hours - burned, 2)
        },
    }
}

/// `budgetSummary` pada `GET /projects` (project dengan budget aktif).
pub fn list_summary(b: &BudgetRow) -> Value {
    let is_hours = b.budget_type != "COST";
    let burned_seconds = f64::from(b.current_burned_seconds);
    let h = hour_figures(burned_seconds, b.budget_hours);
    let budget_amount = b.budget_amount.unwrap_or(0.0);
    let c = cost_figures(b.current_burned_cost, budget_amount);

    let hours = if is_hours {
        json!({
            "budgetHours": num(b.budget_hours),
            "usedHours": num(h.used_hours),
            "remainingHours": num(h.remaining),
            "overBudgetHours": num(h.over_hours),
            "overBudgetPercent": num(h.over_pct),
        })
    } else {
        json!({ "budgetHours": null, "usedHours": null, "remainingHours": null,
                "overBudgetHours": null, "overBudgetPercent": null })
    };
    let cost = if is_hours {
        json!({ "budgetAmount": null, "usedCost": null, "remainingAmount": null,
                "overBudgetAmount": null, "overBudgetPercent": null })
    } else {
        json!({
            "budgetAmount": num(budget_amount),
            "usedCost": num(c.used_cost),
            "remainingAmount": num(c.remaining),
            "overBudgetAmount": num(c.over_amount),
            "overBudgetPercent": num(c.over_pct),
        })
    };
    let real_pct = if is_hours { h.real_pct } else { c.real_pct };
    json!({
        "budgetType": b.budget_type,
        "resetCycle": b.reset_cycle,
        "percentUsed": num(fixed(real_pct.min(100.0), 1)),
        "status": budget_status(real_pct),
        "hours": hours,
        "cost": cost,
    })
}

pub fn cost_summary_text(c: &CostFigures, budget_amount: f64, status: &str) -> String {
    let used = locale(c.used_cost);
    let total = locale(budget_amount);
    let pct = num_string(c.percent_used);
    let remaining = locale(c.remaining);
    if c.is_over {
        format!(
            "Over budget by ${} ({}% above allocation). ${used} used out of ${total} budget.",
            locale(c.over_amount),
            num_string(c.over_pct)
        )
    } else if status == "NEAR_LIMIT" {
        format!(
            "${used} of ${total} used ({pct}%). Only ${remaining} remaining — approaching budget limit."
        )
    } else {
        format!("${used} of ${total} used ({pct}%). ${remaining} remaining.")
    }
}

pub fn hours_summary_text(h: &HourFigures, budget_hours: f64, status: &str) -> String {
    let bh = num_string(budget_hours);
    let used = format_hours(h.used_hours);
    let pct = num_string(h.percent_used);
    if h.is_over {
        format!(
            "Over budget by {} ({}% above allocation). {used} used out of {bh}h budget.",
            format_hours(h.over_hours),
            num_string(h.over_pct)
        )
    } else if status == "NEAR_LIMIT" {
        format!(
            "{used} of {bh}h used ({pct}%). Only {} remaining — approaching budget limit.",
            format_hours(h.remaining)
        )
    } else {
        format!(
            "{used} of {bh}h used ({pct}%). {} remaining.",
            format_hours(h.remaining)
        )
    }
}

/// `getCycleStartDate`; zona waktu proses = UTC.
pub fn cycle_start(
    reset_cycle: &str,
    cycle_start_at: Option<NaiveDateTime>,
    now: NaiveDateTime,
) -> NaiveDateTime {
    let epoch_2000 = NaiveDate::from_ymd_opt(2000, 1, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .unwrap_or(now);
    match reset_cycle {
        "NEVER" => epoch_2000,
        "WEEKLY" => {
            let monday =
                now.date() - Duration::days(i64::from(now.date().weekday().num_days_from_monday()));
            monday.and_hms_opt(0, 0, 0).unwrap_or(now)
        }
        "MONTHLY" => now
            .date()
            .with_day(1)
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .unwrap_or(now),
        _ => cycle_start_at.unwrap_or(epoch_2000),
    }
}

/// `SUM(EXTRACT(EPOCH FROM (endAt - startAt)))` aktif sejak `since`.
async fn burn_seconds(
    state: &AppState,
    project_id: i32,
    since: NaiveDateTime,
    billable_only: bool,
) -> sqlx::Result<f64> {
    sqlx::query_scalar(
        r#"SELECT COALESCE(SUM(EXTRACT(EPOCH FROM ("endAt" - "startAt"))), 0)::float8
           FROM "trackingTimeline"
           WHERE "projectId" = $1 AND "status" = 1
             AND (NOT $3 OR "billable" = true)
             AND "startAt" >= $2"#,
    )
    .bind(project_id)
    .bind(since)
    .bind(billable_only)
    .fetch_one(&state.db)
    .await
}

/// `ProjectBudgetService.findOne(projectUuid)`.
pub async fn find_one(state: &AppState, project_uuid: &str) -> ApiResult<Value> {
    let project: Option<i32> =
        sqlx::query_scalar("SELECT id FROM project WHERE uuid = $1::uuid AND status = 1")
            .bind(project_uuid)
            .fetch_optional(&state.db)
            .await?;
    let Some(project_id) = project else {
        return Ok(json!({ "success": false, "message": "Project not found", "data": null }));
    };
    let budget = fetch_budget(state, project_id).await?;
    let Some(b) = budget.filter(|b| b.is_enabled) else {
        return Ok(json!({ "success": true, "message": "No budget configured", "data": null }));
    };

    let four_weeks_ago = now_naive() - Duration::weeks(4);
    let mut doc = b.doc.clone();
    // `{ ...budget, ... }` Node: skalar menurut urutan model, kunci hitungan sesudahnya.
    rowjson::order(&mut doc, rowjson::PROJECT_BUDGET_KEYS);
    if b.budget_type == "COST" {
        let budget_amount = b.budget_amount.unwrap_or(0.0);
        let c = cost_figures(b.current_burned_cost, budget_amount);
        let status = budget_status(c.real_pct);

        let rate = b.billable_rate.unwrap_or(0.0);
        let burn_rate = if rate == 0.0 {
            0.0
        } else {
            burn_seconds(state, project_id, four_weeks_ago, !b.count_non_billable).await?
                / 3600.0
                / 4.0
                * rate
        };
        let projection = if burn_rate > 0.0 && c.remaining > 0.0 {
            Value::String(format!(
                "At current pace (~${}/week), budget will be reached in ~{} weeks",
                to_fixed(burn_rate, 2),
                to_fixed(c.remaining / burn_rate, 1)
            ))
        } else {
            Value::Null
        };
        rowjson::merge(
            &mut doc,
            vec![
                ("budgetType", json!("COST")),
                ("budgetAmount", num(budget_amount)),
                ("usedCost", num(c.used_cost)),
                ("remainingAmount", num(c.remaining)),
                ("overBudgetAmount", num(c.over_amount)),
                ("overBudgetPercent", num(c.over_pct)),
                ("percentUsed", num(c.percent_used)),
                ("status", json!(status)),
                ("burnRatePerWeek", num(fixed(burn_rate, 2))),
                ("projection", projection),
                (
                    "summary",
                    json!(cost_summary_text(&c, budget_amount, status)),
                ),
            ],
        );
    } else {
        let h = hour_figures(f64::from(b.current_burned_seconds), b.budget_hours);
        let status = budget_status(h.real_pct);
        let burn_rate =
            burn_seconds(state, project_id, four_weeks_ago, false).await? / 3600.0 / 4.0;
        let projection = if burn_rate > 0.0 && h.remaining > 0.0 {
            Value::String(format!(
                "At current pace (~{}/week), budget will be reached in ~{} weeks",
                format_hours(burn_rate),
                to_fixed(h.remaining / burn_rate, 1)
            ))
        } else {
            Value::Null
        };
        rowjson::merge(
            &mut doc,
            vec![
                ("budgetType", json!("HOURS")),
                ("usedHours", num(h.used_hours)),
                ("remainingHours", num(h.remaining)),
                ("overBudgetHours", num(h.over_hours)),
                ("overBudgetPercent", num(h.over_pct)),
                ("percentUsed", num(h.percent_used)),
                ("status", json!(status)),
                ("burnRatePerWeek", num(fixed(burn_rate, 2))),
                ("projection", projection),
                (
                    "summary",
                    json!(hours_summary_text(&h, b.budget_hours, status)),
                ),
            ],
        );
    }
    rowjson::fix(&mut doc);
    Ok(json!({ "success": true, "message": "Success", "data": doc }))
}

/// Hasil `upsert`: baris `projectBudget` tersimpan (bentuk JSON Prisma, SEBELUM
/// `recalculateBurn`) atau pesan penolakan. Route `project-budget` mengembalikan barisnya;
/// create/update project mengabaikannya (seperti Node).
#[derive(Debug, Clone, PartialEq)]
pub enum UpsertOutcome {
    Saved(Value),
    Rejected(&'static str),
}

/// Pure: validasi bisnis awal `upsert` untuk budget COST.
pub fn check_cost_rules(dto: &BudgetInput) -> Option<&'static str> {
    if dto.budget_type.as_deref() == Some("COST") {
        if dto.billable_rate.is_none_or(|r| r <= 0.0) {
            return Some("Billable rate wajib diisi dan lebih dari 0 untuk cost budget");
        }
        if dto.budget_amount.is_none_or(|r| r <= 0.0) {
            return Some("Budget amount wajib diisi dan lebih dari 0 untuk cost budget");
        }
    }
    None
}

/// `isEnabled` efektif: harus ada nilai budget > 0.
pub fn effective_enabled(dto: &BudgetInput) -> bool {
    if dto.budget_type.as_deref() == Some("COST") {
        dto.is_enabled && dto.budget_amount.unwrap_or(0.0) > 0.0
    } else {
        dto.is_enabled && dto.budget_hours.unwrap_or(0.0) > 0.0
    }
}

/// `ProjectBudgetService.upsert(projectUuid, dto)`.
pub async fn upsert(
    state: &AppState,
    project_uuid: &str,
    dto: &BudgetInput,
) -> ApiResult<UpsertOutcome> {
    let project: Option<i32> =
        sqlx::query_scalar("SELECT id FROM project WHERE uuid = $1::uuid AND status = 1")
            .bind(project_uuid)
            .fetch_optional(&state.db)
            .await?;
    let Some(project_id) = project else {
        return Ok(UpsertOutcome::Rejected("Project not found"));
    };
    let budget_type = dto.budget_type.clone().unwrap_or_else(|| "HOURS".into());
    let dto = BudgetInput {
        budget_type: Some(budget_type.clone()),
        ..dto.clone()
    };
    if let Some(msg) = check_cost_rules(&dto) {
        return Ok(UpsertOutcome::Rejected(msg));
    }
    let is_enabled = effective_enabled(&dto);

    let existing = fetch_budget(state, project_id).await?;
    let mut initial_burned_cost = existing.as_ref().map_or(0.0, |e| e.current_burned_cost);
    if let Some(e) = &existing
        && e.budget_type != budget_type
    {
        if budget_type == "COST"
            && let Some(rate) = dto.billable_rate.filter(|r| *r != 0.0)
        {
            initial_burned_cost = f64::from(e.current_burned_seconds) / 3600.0 * rate;
        } else if budget_type == "HOURS" {
            initial_burned_cost = 0.0;
        }
    }

    let now = now_naive();
    let mut doc: Value = sqlx::query_scalar(
        r#"INSERT INTO "projectBudget"
             ("projectId", "isEnabled", "budgetType", "budgetHours", "budgetAmount", "billableRate",
              "countNonBillable", "currentBurnedCost", "resetCycle", "alertThreshold", enforcement,
              "cycleStartAt", "currentBurnedSeconds")
           VALUES ($1, $2, $3::"BudgetType", $4, $5, $6, $7, $8, $9::"BudgetResetCycle", $10,
                   $11::"BudgetEnforcement", $12, 0)
           ON CONFLICT ("projectId") DO UPDATE SET
             "isEnabled" = EXCLUDED."isEnabled", "budgetType" = EXCLUDED."budgetType",
             "budgetHours" = EXCLUDED."budgetHours", "budgetAmount" = EXCLUDED."budgetAmount",
             "billableRate" = EXCLUDED."billableRate", "countNonBillable" = EXCLUDED."countNonBillable",
             "currentBurnedCost" = EXCLUDED."currentBurnedCost", "resetCycle" = EXCLUDED."resetCycle",
             "alertThreshold" = EXCLUDED."alertThreshold", enforcement = EXCLUDED.enforcement,
             "updatedAt" = $12, "thresholdAlertSentAt" = NULL, "budgetReachedSentAt" = NULL
           RETURNING to_jsonb("projectBudget".*)"#,
    )
    .bind(project_id)
    .bind(is_enabled)
    .bind(&budget_type)
    .bind(dto.budget_hours.unwrap_or(0.0))
    .bind(dto.budget_amount)
    .bind(dto.billable_rate)
    .bind(dto.count_non_billable.unwrap_or(true))
    .bind(initial_burned_cost)
    .bind(dto.reset_cycle.as_deref().unwrap_or("NEVER"))
    .bind(dto.alert_threshold.unwrap_or(80))
    .bind(dto.enforcement.as_deref().unwrap_or("ALERT_ONLY"))
    .bind(now)
    .fetch_one(&state.db)
    .await?;
    rowjson::order(&mut doc, rowjson::PROJECT_BUDGET_KEYS);
    rowjson::fix(&mut doc);

    if is_enabled {
        recalculate_burn(state, project_id).await?;
    }
    Ok(UpsertOutcome::Saved(doc))
}

/// `recalculateBurn(projectId)`.
pub async fn recalculate_burn(state: &AppState, project_id: i32) -> ApiResult<()> {
    let Some(budget) = fetch_budget(state, project_id)
        .await?
        .filter(|b| b.is_enabled)
    else {
        return Ok(());
    };
    if budget.budget_hours == 0.0 && budget.budget_amount.unwrap_or(0.0) == 0.0 {
        return Ok(());
    }
    let now = now_naive();
    let start = cycle_start(&budget.reset_cycle, budget.cycle_start_at.map(|d| d.0), now);
    let total = burn_seconds(state, project_id, start, !budget.count_non_billable).await?;
    // Kolom Int: Prisma memotong ke arah nol; biaya memakai total mentah (seperti Node).
    let seconds_i32 = jsfmt::as_i32(total).unwrap_or(0);
    let burned_cost = match budget.billable_rate.filter(|r| *r != 0.0) {
        Some(rate) if budget.budget_type == "COST" => total / 3600.0 * rate,
        _ => 0.0,
    };
    sqlx::query(
        r#"UPDATE "projectBudget" SET "currentBurnedSeconds" = $2, "currentBurnedCost" = $3, "updatedAt" = $4
           WHERE "projectId" = $1"#,
    )
    .bind(project_id)
    .bind(seconds_i32)
    .bind(burned_cost)
    .bind(now)
    .execute(&state.db)
    .await?;

    let updated = BudgetRow {
        current_burned_seconds: seconds_i32,
        current_burned_cost: burned_cost,
        ..budget
    };
    check_and_fire_alerts(state, &updated).await
}

/// Persen terpakai untuk alert; `None` = tidak ada yang dicek (budget 0).
pub fn alert_percent(b: &BudgetRow) -> Option<f64> {
    let burned_hours = f64::from(b.current_burned_seconds) / 3600.0;
    if b.budget_type == "COST" {
        let amount = b.budget_amount.unwrap_or(0.0);
        if amount == 0.0 {
            return None;
        }
        Some(b.current_burned_cost / amount * 100.0)
    } else {
        if b.budget_hours == 0.0 {
            return None;
        }
        Some(burned_hours / b.budget_hours * 100.0)
    }
}

/// `checkAndFireAlerts(budget)`.
pub async fn check_and_fire_alerts(state: &AppState, b: &BudgetRow) -> ApiResult<()> {
    if !b.is_enabled {
        return Ok(());
    }
    let Some(percent_used) = alert_percent(b) else {
        return Ok(());
    };
    let burned_hours = f64::from(b.current_burned_seconds) / 3600.0;
    let burned_cost = b.current_burned_cost;
    let is_cost = b.budget_type == "COST";

    let project: Option<(String, Option<String>)> = sqlx::query_as(
        r#"SELECT p.name,
             (SELECT c."budgetAlertDeliveryChannel"::text FROM "projectCompany" pc
                JOIN company c ON c.id = pc."companyId" WHERE pc."projectId" = p.id
                ORDER BY pc.id LIMIT 1)
           FROM project p WHERE p.id = $1"#,
    )
    .bind(b.project_id)
    .fetch_optional(&state.db)
    .await?;
    let project_name = project.as_ref().map(|p| p.0.clone()).unwrap_or_default();
    let send_email = project.as_ref().and_then(|p| p.1.as_deref()) == Some("EMAIL_SLACK");

    let managers: Vec<(String, String)> = sqlx::query_as(
        r#"SELECT u.username, u.name FROM "projectManualTimeApprover" a
           JOIN users u ON u.id = a."userId"
           WHERE a."projectId" = $1 AND a.status = 1 ORDER BY a.id"#,
    )
    .bind(b.project_id)
    .fetch_all(&state.db)
    .await?;

    let now = now_naive();
    let start = cycle_start(&b.reset_cycle, b.cycle_start_at.map(|d| d.0), now);
    let not_sent = |sent: Option<JsDate>| sent.is_none_or(|s| s.0 < start);

    // Alert 1: threshold.
    if percent_used >= f64::from(b.alert_threshold)
        && not_sent(b.threshold_alert_sent_at)
        && percent_used < 100.0
    {
        mark_sent(state, b, "thresholdAlertSentAt", "THRESHOLD", percent_used).await?;
        if send_email {
            for (email, name) in &managers {
                let burned_hours_r = fixed(burned_hours, 1);
                let percent_r = fixed(percent_used, 0);
                let cost_r = fixed(burned_cost, 2);
                let subject = if is_cost {
                    format!(
                        "Project '{project_name}' has used {}% of its budget (${} of ${})",
                        num_string(percent_r),
                        to_fixed(cost_r, 0),
                        b.budget_amount
                            .map_or_else(|| "undefined".to_owned(), |a| to_fixed(a, 0))
                    )
                } else {
                    format!(
                        "Project '{project_name}' has used {}% of its budget",
                        num_string(percent_r)
                    )
                };
                let context = json!({
                    "name": name, "projectName": project_name,
                    "burnedHours": num(burned_hours_r), "budgetHours": num(b.budget_hours),
                    "percentUsed": num(percent_r), "budgetType": b.budget_type,
                    "burnedCost": if is_cost { num(cost_r) } else { Value::Null },
                    "budgetAmount": if is_cost { b.budget_amount.map_or(Value::Null, num) } else { Value::Null },
                    "isCostBudget": is_cost, "appUrl": state.config.var("HOST_FE"),
                });
                let _ = send(
                    state,
                    Mail {
                        to_email: email,
                        to_name: name,
                        subject: &subject,
                        text: None,
                        template: "budget-threshold-alert",
                        context,
                        category: "Budget Threshold Alert",
                        label: "send budget threshold alert",
                    },
                )
                .await;
            }
        }
    }

    // Alert 2: budget tercapai (100%).
    if percent_used >= 100.0 && not_sent(b.budget_reached_sent_at) {
        mark_sent(
            state,
            b,
            "budgetReachedSentAt",
            "BUDGET_REACHED",
            percent_used,
        )
        .await?;
        if send_email {
            for (email, name) in &managers {
                let subject = format!("Project '{project_name}' has reached its budget");
                let context = json!({
                    "name": name, "projectName": project_name,
                    "burnedHours": num(fixed(burned_hours, 1)), "budgetHours": num(b.budget_hours),
                    "isStopped": b.enforcement == "STOP_TRACKING", "budgetType": b.budget_type,
                    "burnedCost": if is_cost { num(fixed(burned_cost, 2)) } else { Value::Null },
                    "budgetAmount": if is_cost { b.budget_amount.map_or(Value::Null, num) } else { Value::Null },
                    "isCostBudget": is_cost, "appUrl": state.config.var("HOST_FE"),
                });
                let _ = send(
                    state,
                    Mail {
                        to_email: email,
                        to_name: name,
                        subject: &subject,
                        text: None,
                        template: "budget-reached-alert",
                        context,
                        category: "Budget Reached Alert",
                        label: "send budget reached alert",
                    },
                )
                .await;
            }
        }
    }
    Ok(())
}

/// Tandai alert terkirim + catat baris `projectBudgetAlert`.
async fn mark_sent(
    state: &AppState,
    b: &BudgetRow,
    column: &'static str,
    alert_type: &'static str,
    percent_used: f64,
) -> ApiResult<()> {
    let now = now_naive();
    // `column` selalu konstanta di atas, bukan input pengguna.
    sqlx::query(&format!(
        r#"UPDATE "projectBudget" SET "{column}" = $2, "updatedAt" = $2 WHERE "projectId" = $1"#
    ))
    .bind(b.project_id)
    .bind(now)
    .execute(&state.db)
    .await?;
    let is_cost = b.budget_type == "COST";
    sqlx::query(
        r#"INSERT INTO "projectBudgetAlert"
             ("projectBudgetId", "alertType", "burnedHours", "budgetHours", "percentUsed",
              "budgetType", "burnedCost", "budgetAmount")
           VALUES ($1, $2::"BudgetAlertType", $3, $4, $5, $6::"BudgetType", $7, $8)"#,
    )
    .bind(b.id)
    .bind(alert_type)
    .bind(f64::from(b.current_burned_seconds) / 3600.0)
    .bind(b.budget_hours)
    .bind(percent_used)
    .bind(&b.budget_type)
    .bind(is_cost.then_some(b.current_burned_cost))
    .bind(if is_cost { b.budget_amount } else { None })
    .execute(&state.db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(budget_type: &str, hours: f64, secs: i32, amount: Option<f64>, cost: f64) -> BudgetRow {
        BudgetRow {
            id: 1,
            project_id: 1,
            is_enabled: true,
            budget_type: budget_type.into(),
            billable_rate: Some(50.0),
            budget_amount: amount,
            count_non_billable: true,
            current_burned_cost: cost,
            budget_hours: hours,
            reset_cycle: "NEVER".into(),
            alert_threshold: 80,
            enforcement: "ALERT_ONLY".into(),
            cycle_start_at: None,
            current_burned_seconds: secs,
            threshold_alert_sent_at: None,
            budget_reached_sent_at: None,
            doc: Value::Null,
        }
    }

    #[test]
    fn status_thresholds() {
        assert_eq!(budget_status(89.9), "ON_TRACK");
        assert_eq!(budget_status(90.0), "NEAR_LIMIT");
        assert_eq!(budget_status(100.0), "OVER_BUDGET");
    }

    #[test]
    fn hour_figures_under_and_over() {
        let h = hour_figures(3600.0 * 5.0, 10.0);
        assert_eq!(
            (h.used_hours, h.remaining, h.percent_used, h.is_over),
            (5.0, 5.0, 50.0, false)
        );
        let o = hour_figures(3600.0 * 12.0, 10.0);
        assert!(o.is_over);
        assert_eq!(
            (o.remaining, o.over_hours, o.over_pct, o.percent_used),
            (0.0, 2.0, 20.0, 100.0)
        );
    }

    #[test]
    fn zero_budget_guards() {
        let h = hour_figures(7200.0, 0.0);
        assert!(h.is_over);
        assert_eq!((h.real_pct, h.over_pct), (0.0, 0.0));
        let c = cost_figures(10.0, 0.0);
        assert_eq!((c.real_pct, c.over_pct), (0.0, 0.0));
    }

    #[test]
    fn cost_figures_rounding() {
        let c = cost_figures(123.456, 1000.0);
        assert_eq!(
            (c.used_cost, c.remaining, c.percent_used),
            (123.46, 876.54, 12.3)
        );
    }

    #[test]
    fn summary_texts() {
        let c = cost_figures(1500.5, 1000.0);
        assert_eq!(
            cost_summary_text(&c, 1000.0, "OVER_BUDGET"),
            "Over budget by $500.5 (50% above allocation). $1,500.5 used out of $1,000 budget."
        );
        let h = hour_figures(3600.0 * 9.5, 10.0);
        assert_eq!(
            hours_summary_text(&h, 10.0, "NEAR_LIMIT"),
            "9h 30m of 10h used (95%). Only 0h 30m remaining — approaching budget limit."
        );
        let h = hour_figures(3600.0 * 2.0, 10.0);
        assert_eq!(
            hours_summary_text(&h, 10.0, "ON_TRACK"),
            "2h of 10h used (20%). 8h remaining."
        );
    }

    #[test]
    fn list_summary_shapes() {
        let s = list_summary(&row("HOURS", 10.0, 18_000, None, 0.0));
        assert_eq!(s["hours"]["usedHours"], 5);
        assert_eq!(s["cost"]["usedCost"], Value::Null);
        assert_eq!(s["status"], "ON_TRACK");
        let s = list_summary(&row("COST", 0.0, 0, Some(100.0), 150.0));
        assert_eq!(s["hours"]["budgetHours"], Value::Null);
        assert_eq!(s["cost"]["overBudgetAmount"], 50);
        assert_eq!(s["status"], "OVER_BUDGET");
        assert_eq!(s["percentUsed"], 100);
    }

    #[test]
    fn cycle_start_dates() {
        let now = NaiveDate::from_ymd_opt(2026, 11, 18)
            .and_then(|d| d.and_hms_opt(10, 30, 0))
            .unwrap_or_default();
        assert_eq!(
            cycle_start("NEVER", None, now).to_string(),
            "2000-01-01 00:00:00"
        );
        assert_eq!(
            cycle_start("WEEKLY", None, now).to_string(),
            "2026-11-16 00:00:00"
        );
        assert_eq!(
            cycle_start("MONTHLY", None, now).to_string(),
            "2026-11-01 00:00:00"
        );
    }

    #[test]
    fn upsert_rules() {
        let mut d = BudgetInput {
            is_enabled: true,
            budget_type: Some("COST".into()),
            ..Default::default()
        };
        assert!(check_cost_rules(&d).is_some_and(|m| m.starts_with("Billable rate")));
        d.billable_rate = Some(10.0);
        assert!(check_cost_rules(&d).is_some_and(|m| m.starts_with("Budget amount")));
        d.budget_amount = Some(100.0);
        assert!(check_cost_rules(&d).is_none());
        assert!(effective_enabled(&d));
        let h = BudgetInput {
            is_enabled: true,
            budget_hours: Some(0.0),
            ..Default::default()
        };
        assert!(!effective_enabled(&h));
    }

    #[test]
    fn alert_percent_cases() {
        assert_eq!(
            alert_percent(&row("HOURS", 10.0, 3600 * 8, None, 0.0)),
            Some(80.0)
        );
        assert_eq!(alert_percent(&row("HOURS", 0.0, 3600, None, 0.0)), None);
        assert_eq!(
            alert_percent(&row("COST", 0.0, 0, Some(200.0), 50.0)),
            Some(25.0)
        );
    }
}

#[cfg(test)]
mod can_start_tests {
    use super::can_start_tracking_block as b;

    #[test]
    fn mirrors_can_start_tracking() {
        let msg = Some("This project has reached its budget limit. Contact your manager.");
        let hrs = |role, burned, enf| {
            b(
                role,
                Some(true),
                Some("HOURS"),
                None,
                None,
                Some(1.0),
                Some(burned),
                Some(enf),
            )
        };
        assert_eq!(hrs("STAFF", 3600, "STOP_TRACKING"), msg);
        assert_eq!(hrs("STAFF", 3599, "STOP_TRACKING"), None);
        assert_eq!(hrs("STAFF", 3600, "WARN"), None);
        assert_eq!(hrs("MANAGER", 3600, "STOP_TRACKING"), None);
        assert_eq!(
            b(
                "STAFF",
                Some(false),
                Some("HOURS"),
                None,
                None,
                Some(1.0),
                Some(9999),
                Some("STOP_TRACKING")
            ),
            None
        );
        assert_eq!(
            b(
                "STAFF",
                Some(true),
                Some("COST"),
                Some(10.0),
                Some(10.0),
                None,
                None,
                Some("STOP_TRACKING")
            ),
            msg
        );
        assert_eq!(
            b(
                "STAFF",
                Some(true),
                Some("COST"),
                Some(0.0),
                Some(10.0),
                None,
                None,
                Some("STOP_TRACKING")
            ),
            None
        );
        assert_eq!(
            b(
                "STAFF",
                Some(true),
                Some("HOURS"),
                None,
                None,
                Some(0.0),
                Some(10),
                Some("STOP_TRACKING")
            ),
            None
        );
    }
}
