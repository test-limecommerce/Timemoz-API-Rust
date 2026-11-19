//! Port `src/project-budget` (4 route: `ProjectBudgetController` + `BudgetReportController`).
//! Logika angka/ringkasan dipakai ulang dari `mz_projects::budget`; cron TIDAK di-port.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use mz_core::company::require_company_id;
use mz_core::extract::{PathParams, Validate, ValidatedJson};
use mz_core::jsfmt::{num, num_string};
use mz_core::time::now_naive;
use mz_core::{ApiResult, AppState, RequestCtx};
use mz_projects::budget::{
    self, budget_status, cost_figures, cost_summary_text, hour_figures, hours_summary_text,
};
use mz_projects::dto::{BudgetInput, validate_budget};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::util::{failure, parse_uuid};

/// Body `UpsertProjectBudgetDto`.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct BudgetBody(pub Map<String, Value>);

impl Validate for BudgetBody {
    fn validate(&self) -> Vec<String> {
        validate_budget(&self.0)
    }
}

async fn project_id(state: &AppState, uuid: &str) -> ApiResult<Option<i32>> {
    Ok(
        sqlx::query_scalar("SELECT id FROM project WHERE uuid = $1::uuid AND status = 1")
            .bind(uuid)
            .fetch_optional(&state.db)
            .await?,
    )
}

/// Node: src/project-budget/project-budget.service.ts:123 (`upsert`): `mz_projects::budget::upsert`.
pub async fn upsert(state: &AppState, uuid: &str, dto: &BudgetInput) -> ApiResult<Value> {
    Ok(match budget::upsert(state, uuid, dto).await? {
        budget::UpsertOutcome::Saved(doc) => {
            json!({ "success": true, "message": "Budget saved", "data": doc })
        }
        budget::UpsertOutcome::Rejected(msg) => failure(msg),
    })
}

/// Node: src/project-budget/project-budget.service.ts:200 (`remove`).
pub async fn remove(state: &AppState, uuid: &str) -> ApiResult<Value> {
    let Some(project_id) = project_id(state, uuid).await? else {
        return Ok(failure("Project not found"));
    };
    sqlx::query(r#"UPDATE "projectBudget" SET "isEnabled" = false, "updatedAt" = $2 WHERE "projectId" = $1"#)
        .bind(project_id)
        .bind(now_naive())
        .execute(&state.db)
        .await?;
    Ok(json!({ "success": true, "message": "Budget disabled", "data": { "isDisabled": true } }))
}

#[derive(Debug, sqlx::FromRow)]
struct ReportRow {
    budget_type: String,
    current_burned_cost: f64,
    budget_amount: Option<f64>,
    billable_rate: Option<f64>,
    budget_hours: f64,
    current_burned_seconds: i32,
    reset_cycle: String,
    count_non_billable: bool,
    project_uuid: String,
    project_name: String,
    project_code: String,
}

/// Satu baris laporan (`getBudgetReport` map).
fn report_item(b: &ReportRow) -> (Value, &'static str) {
    if b.budget_type == "COST" {
        let amount = b.budget_amount.unwrap_or(0.0);
        let c = cost_figures(b.current_burned_cost, amount);
        let status = budget_status(c.real_pct);
        let item = json!({
            "projectUuid": b.project_uuid,
            "projectName": b.project_name,
            "projectCode": b.project_code,
            "budgetType": "COST",
            "budgetAmount": num(amount),
            "usedCost": num(c.used_cost),
            "remainingAmount": num(c.remaining),
            "overBudgetAmount": num(c.over_amount),
            "overBudgetPercent": num(c.over_pct),
            "billableRate": b.billable_rate.map_or(Value::Null, num),
            "percentUsed": num(c.percent_used),
            "status": status,
            "resetCycle": b.reset_cycle,
            "countNonBillable": b.count_non_billable,
            "summary": cost_summary_text(&c, amount, status),
        });
        return (item, status);
    }
    let h = hour_figures(f64::from(b.current_burned_seconds), b.budget_hours);
    let status = budget_status(h.real_pct);
    let item = json!({
        "projectUuid": b.project_uuid,
        "projectName": b.project_name,
        "projectCode": b.project_code,
        "budgetType": "HOURS",
        "budgetHours": num(b.budget_hours),
        "usedHours": num(h.used_hours),
        "remainingHours": num(h.remaining),
        "overBudgetHours": num(h.over_hours),
        "overBudgetPercent": num(h.over_pct),
        "percentUsed": num(h.percent_used),
        "status": status,
        "resetCycle": b.reset_cycle,
        "countNonBillable": b.count_non_billable,
        "summary": hours_summary_text(&h, b.budget_hours, status),
    });
    (item, status)
}

/// Node: src/project-budget/project-budget.service.ts:219 (`getBudgetReport`).
pub async fn budget_report(
    state: &AppState,
    company_id: i32,
    status_filter: Option<&str>,
) -> ApiResult<Vec<Value>> {
    let rows: Vec<ReportRow> = sqlx::query_as(
        r#"SELECT b."budgetType"::text AS budget_type, b."currentBurnedCost" AS current_burned_cost,
                  b."budgetAmount" AS budget_amount, b."billableRate" AS billable_rate,
                  b."budgetHours" AS budget_hours, b."currentBurnedSeconds" AS current_burned_seconds,
                  b."resetCycle"::text AS reset_cycle, b."countNonBillable" AS count_non_billable,
                  p.uuid::text AS project_uuid, p.name AS project_name, p.code AS project_code
           FROM "projectBudget" b JOIN project p ON p.id = b."projectId"
           WHERE b."isEnabled" = true AND p.status = 1
             AND EXISTS (SELECT 1 FROM "projectCompany" pc WHERE pc."projectId" = p.id AND pc."companyId" = $1)
           ORDER BY b.id"#,
    )
    .bind(company_id)
    .fetch_all(&state.db)
    .await?;
    let filter = status_filter.filter(|s| !s.is_empty() && *s != "ALL");
    Ok(rows
        .iter()
        .map(report_item)
        .filter(|(_, status)| filter.is_none_or(|f| f == *status))
        .map(|(item, _)| item)
        .collect())
}

fn js_str(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.as_f64().map_or_else(|| n.to_string(), num_string),
        Some(other) => other.to_string(),
    }
}

/// CSV `export=csv`. Node (`project-budget.controller.ts:111`) membaca properti yang
/// tidak ada di item laporan (`burnedCost`, `burnedHours`, `remaining`) sehingga kolom
/// Burned/Remaining berisi "undefined". Di sini dibaca dari field yang benar-benar
/// dikirim `report_item`: `usedCost`/`remainingAmount` dan `usedHours`/`remainingHours`.
/// Dua perbaikan lain terhadap Node: `"` di nama proyek di-escape jadi `""` (RFC 4180;
/// Node menulis apa adanya sehingga kolom bergeser), dan `billableRate` null pada COST
/// ditulis `N/A` seperti baris HOURS (Node: `$null`).
pub fn report_csv(rows: &[Value]) -> String {
    let header = "Project,Type,Budget,Burned,Remaining,% Used,Status,Reset Cycle,Billable Rate,Count Non-Billable";
    let lines = rows.iter().map(|r| {
        let yes = if r.get("countNonBillable").and_then(Value::as_bool) == Some(true) {
            "Yes"
        } else {
            "No"
        };
        let name = js_str(r.get("projectName")).replace('"', "\"\"");
        let pct = js_str(r.get("percentUsed"));
        let status = js_str(r.get("status"));
        let cycle = js_str(r.get("resetCycle"));
        if r.get("budgetType").and_then(Value::as_str) == Some("COST") {
            let rate = match r.get("billableRate") {
                Some(v @ Value::Number(_)) => format!("${}", js_str(Some(v))),
                _ => "N/A".to_owned(),
            };
            format!(
                "\"{name}\",COST,${},${},${},{pct}%,{status},{cycle},{rate},{yes}",
                js_str(r.get("budgetAmount")),
                js_str(r.get("usedCost")),
                js_str(r.get("remainingAmount")),
            )
        } else {
            format!(
                "\"{name}\",HOURS,{}h,{}h,{}h,{pct}%,{status},{cycle},N/A,{yes}",
                js_str(r.get("budgetHours")),
                js_str(r.get("usedHours")),
                js_str(r.get("remainingHours")),
            )
        }
    });
    std::iter::once(header.to_owned())
        .chain(lines)
        .collect::<Vec<_>>()
        .join("\n")
}

// ───────────────────────── handler HTTP ─────────────────────────

/// Node: src/project-budget/project-budget.controller.ts:35 (`findOne`)
pub async fn h_find_one(
    State(state): State<AppState>,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    Ok(Json(budget::find_one(&state, &uuid).await?))
}

/// Node: src/project-budget/project-budget.controller.ts:42 (`upsert`, `@HttpCode(200)`)
pub async fn h_upsert(
    State(state): State<AppState>,
    PathParams(uuid): PathParams<String>,
    ValidatedJson(body): ValidatedJson<BudgetBody>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    let dto = BudgetInput::from_map(&body.0);
    Ok(Json(upsert(&state, &uuid, &dto).await?))
}

/// Node: src/project-budget/project-budget.controller.ts:56 (`remove`, `@HttpCode(200)`)
pub async fn h_remove(
    State(state): State<AppState>,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    Ok(Json(remove(&state, &uuid).await?))
}

/// Query `GET /budget-report` (`status`, `export` dibaca mentah).
#[derive(Debug, Default, Deserialize)]
pub struct ReportQuery {
    pub status: Option<String>,
    pub export: Option<String>,
}

/// Node: src/project-budget/project-budget.controller.ts:79 (`getBudgetReport`)
pub async fn h_report(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Query(q): Query<ReportQuery>,
) -> ApiResult<Response> {
    let user_id = ctx.require_user_id()?;
    let company_id = require_company_id(&state.db, user_id).await?;
    let rows = budget_report(&state, company_id, q.status.as_deref()).await?;
    if q.export.as_deref() == Some("csv") {
        let mut res = (StatusCode::OK, report_csv(&rows)).into_response();
        let h = res.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/csv; charset=utf-8"),
        );
        h.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment; filename=\"budget-report.csv\""),
        );
        return Ok(res);
    }
    Ok(Json(json!({ "success": true, "message": "Success", "data": rows })).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(t: &str, burned_secs: i32, cost: f64) -> ReportRow {
        ReportRow {
            budget_type: t.into(),
            current_burned_cost: cost,
            budget_amount: Some(100.0),
            billable_rate: Some(10.0),
            budget_hours: 10.0,
            current_burned_seconds: burned_secs,
            reset_cycle: "NEVER".into(),
            count_non_billable: true,
            project_uuid: "u".into(),
            project_name: "P".into(),
            project_code: "C".into(),
        }
    }

    #[test]
    fn hours_item_shape() {
        let (v, status) = report_item(&row("HOURS", 18_000, 0.0));
        assert_eq!(status, "ON_TRACK");
        assert_eq!(v["usedHours"], json!(5));
        assert_eq!(v["remainingHours"], json!(5));
        assert_eq!(v["percentUsed"], json!(50));
    }

    #[test]
    fn cost_item_over_budget() {
        let (v, status) = report_item(&row("COST", 0, 150.0));
        assert_eq!(status, "OVER_BUDGET");
        assert_eq!(v["overBudgetAmount"], json!(50));
        assert_eq!(v["percentUsed"], json!(100));
        assert_eq!(v["remainingAmount"], json!(0));
    }

    #[test]
    fn csv_fills_burned_and_remaining_columns() {
        let (c, _) = report_item(&row("COST", 0, 50.0));
        let (h, _) = report_item(&row("HOURS", 3600, 0.0));
        let csv = report_csv(&[c, h]);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines.get(1).copied(),
            Some("\"P\",COST,$100,$50,$50,50%,ON_TRACK,NEVER,$10,Yes")
        );
        assert_eq!(
            lines.get(2).copied(),
            Some("\"P\",HOURS,10h,1h,9h,10%,ON_TRACK,NEVER,N/A,Yes")
        );
        assert!(!csv.contains("undefined"), "{csv}");
    }

    #[test]
    fn csv_escapes_quotes_and_null_rate() {
        let mut r = row("COST", 0, 50.0);
        r.project_name = "Proyek \"Alpha\", Tahap 1".into();
        r.billable_rate = None;
        let (c, _) = report_item(&r);
        let csv = report_csv(&[c]);
        assert_eq!(
            csv.lines().nth(1),
            Some("\"Proyek \"\"Alpha\"\", Tahap 1\",COST,$100,$50,$50,50%,ON_TRACK,NEVER,N/A,Yes")
        );
        assert!(!csv.contains("null"), "{csv}");
    }
}
