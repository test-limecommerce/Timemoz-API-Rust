//! `POST /admin/sync/trigger` (`admin/admin.controller.ts:190`) dan
//! `POST /admin/manager-summary/test` (`admin/admin.controller.ts:331` -> `TasksService.sendManagerSummaryTest`, `tasks.service.ts:3186`).
//!
//! Guard: `JwtAdminAuthGuard` (extractor [`Operator`]), `@UsePipes(ADMIN_VALIDATION_PIPE)`
//! (whitelist + forbidNonWhitelisted), `OperatorAuditInterceptor` (audit hanya bila respons tidak ber-`success: false`).
//! Tanpa `@OperatorRoles` (semua operator aktif boleh) dan tanpa `@HttpCode` (POST = 201).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::{Days, NaiveDate, Utc};
use mz_core::extract::is_email;
use mz_core::jsfmt::number as js_number;
use mz_core::{ApiError, ApiResult, AppState};
use mz_operator::{AuditCtx, AuditMeta, CompanyFrom, Operator, audit_after_success};
use mz_timesheets::jobs::{
    Collected, SummaryParams, collect_daily_summary, collect_weekly_summary, mail_context,
};
use mz_timesheets::mail::{SummaryKind, send_manager_summary};
use serde_json::{Value, json};

use crate::sync::run_sync_for_date;

/// Body mentah ala `express.json` (strict) — satu implementasi di `mz_core::extract`.
pub use mz_core::extract::RawBody;

/// `@Matches(/^\d{4}-\d{2}-\d{2}$/)`.
fn is_ymd(v: &Value) -> bool {
    let Some(s) = v.as_str() else { return false };
    let b = s.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                *c == b'-'
            } else {
                c.is_ascii_digit()
            }
        })
}

fn unknown_props(body: &Value, allowed: &[&str], errors: &mut Vec<String>) {
    match body {
        Value::Object(m) => {
            for k in m.keys() {
                if !allowed.contains(&k.as_str()) {
                    errors.push(format!("property {k} should not exist"));
                }
            }
        }
        Value::Array(a) => {
            for i in 0..a.len() {
                errors.push(format!("property {i} should not exist"));
            }
        }
        _ => {}
    }
}

fn parse_ymd(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// `SyncTriggerDto`: `@Matches` + `@IsDateString({ strict: true })`. Pesan: class-validator
/// mengurutkan dekorator dari bawah ke atas (IsDateString dulu, lalu Matches).
pub fn validate_sync_trigger(body: &Value) -> Result<NaiveDate, ApiError> {
    let mut errors = Vec::new();
    unknown_props(body, &["date"], &mut errors);
    let date = body.get("date");
    let valid_date = date.and_then(Value::as_str).and_then(parse_ymd);
    let matches = date.is_some_and(is_ymd);
    if valid_date.is_none() {
        // IsDateString (strict) gagal: bentuk salah atau tanggal kalender tidak ada.
        errors.push("date must be a valid ISO 8601 date string".to_owned());
    }
    if !matches {
        errors.push("date must be YYYY-MM-DD".to_owned());
    }
    match (errors.is_empty(), valid_date) {
        (true, Some(d)) => Ok(d),
        _ => Err(ApiError::Validation(errors)),
    }
}

/// Body ter-validasi `ManagerSummaryTestDto`.
#[derive(Debug, PartialEq)]
pub struct SummaryTest {
    pub company_id: i32,
    pub daily: bool,
    pub date: Option<String>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub emails: Option<Vec<String>>,
}

/// `@Type(() => Number)` (class-transformer `Number(value)`).
fn to_number(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => Some(js_number(s)),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// `ManagerSummaryTestDto` di bawah `ADMIN_VALIDATION_PIPE`. Urutan: properti asing, lalu per
/// properti sesuai deklarasi; pesan per properti dari dekorator paling bawah ke atas.
pub fn validate_summary_test(body: &Value) -> Result<SummaryTest, ApiError> {
    let mut errors = Vec::new();
    unknown_props(
        body,
        &[
            "companyId",
            "type",
            "date",
            "startDate",
            "endDate",
            "emails",
        ],
        &mut errors,
    );
    let field = |k: &str| body.get(k).filter(|v| !v.is_null());

    let n = to_number(body.get("companyId"));
    let is_int = n.is_some_and(|x| x.is_finite() && x.fract() == 0.0);
    if !n.is_some_and(|x| x >= 1.0) {
        errors.push("companyId must not be less than 1".to_owned());
    }
    if !is_int {
        errors.push("companyId must be an integer number".to_owned());
    }
    let company_id = n
        .filter(|x| is_int && *x >= 1.0)
        .and_then(mz_core::jsfmt::as_i32);

    let ty = body.get("type").and_then(Value::as_str);
    let daily = ty == Some("daily");
    if !daily && ty != Some("weekly") {
        errors.push("type must be one of the following values: daily, weekly".to_owned());
    }

    let mut ymd = |k: &str| -> Option<String> {
        let v = field(k)?;
        if is_ymd(v) {
            v.as_str().map(str::to_owned)
        } else {
            errors.push(format!("{k} must be YYYY-MM-DD"));
            None
        }
    };
    let date = ymd("date");
    let start_date = ymd("startDate");
    let end_date = ymd("endDate");

    let mut emails = None;
    if let Some(v) = field("emails") {
        // Urutan dekorator bawah->atas: IsEmail(each), ArrayMaxSize(20), IsArray.
        match v {
            Value::Array(a) => {
                if !a.iter().all(|e| e.as_str().is_some_and(is_email)) {
                    errors.push("each value in emails must be an email".to_owned());
                }
                if a.len() > 20 {
                    errors.push("emails must contain no more than 20 elements".to_owned());
                }
                emails = Some(
                    a.iter()
                        .filter_map(|e| e.as_str().map(str::to_owned))
                        .collect(),
                );
            }
            other => {
                if !other.as_str().is_some_and(is_email) {
                    errors.push("each value in emails must be an email".to_owned());
                }
                errors.push("emails must contain no more than 20 elements".to_owned());
                errors.push("emails must be an array".to_owned());
            }
        }
    }

    match (errors.is_empty(), company_id) {
        (true, Some(company_id)) => Ok(SummaryTest {
            company_id,
            daily,
            date,
            start_date,
            end_date,
            emails,
        }),
        _ => Err(ApiError::Validation(errors)),
    }
}

/// Node: src/admin/admin.controller.ts:190 (`triggerManualDailySync`).
pub async fn sync_trigger(
    State(state): State<AppState>,
    op: Operator,
    audit: AuditCtx,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let date = validate_sync_trigger(&body)?;
    let res = run_sync_for_date(&state.db, date).await;
    audit_after_success(
        &state,
        &op,
        &audit,
        AuditMeta {
            action: "sync.trigger",
            target_type: "system",
            company_from: None,
        },
        None,
        Some(&body),
    )
    .await;
    Ok((StatusCode::CREATED, Json(res)))
}

#[derive(sqlx::FromRow)]
struct ConfigRow {
    emails: Value,
    user_ids: Option<Value>,
    low_hours_threshold: i32,
    disabled_flags: Option<Value>,
    include_weekends: bool,
    company_name: String,
}

/// Aturan sama dengan cron manager summary (`SummaryParams::from_config`).
fn params(c: &ConfigRow) -> SummaryParams {
    SummaryParams::from_config(
        c.user_ids.as_ref(),
        c.low_hours_threshold,
        c.disabled_flags.as_ref(),
        c.include_weekends,
    )
}

fn fail(message: &str) -> Value {
    json!({ "success": false, "message": message })
}

/// Node: `tasks.service.ts:3186` `sendManagerSummaryTest`. Tidak mengubah `lastDailySentAt`/`lastWeeklySentAt`;
/// hasil kirim email diabaikan (Node selalu `success: true` sesudah memanggil `sendManagerSummary*`).
pub async fn send_manager_summary_test(state: &AppState, p: &SummaryTest) -> anyhow::Result<Value> {
    let cfg: Option<ConfigRow> = sqlx::query_as(
        r#"SELECT m.emails, m."userIds" AS user_ids, m."lowHoursThreshold" AS low_hours_threshold,
                  m."disabledFlags" AS disabled_flags, m."includeWeekends" AS include_weekends,
                  c.name AS company_name
           FROM "managerSummaryConfig" m JOIN company c ON c.id = m."companyId"
           WHERE m."companyId" = $1"#,
    )
    .bind(p.company_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(cfg) = cfg else {
        return Ok(fail("Manager summary config not found for this company"));
    };
    let config_emails: Vec<String> = cfg
        .emails
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let emails = match &p.emails {
        Some(e) if !e.is_empty() => e.clone(),
        _ => config_emails,
    };
    if emails.is_empty() {
        return Ok(fail("No email recipients configured"));
    }
    let yesterday = Utc::now()
        .date_naive()
        .checked_sub_days(Days::new(1))
        .unwrap_or_default();
    let ymd = |d: NaiveDate| d.format("%Y-%m-%d").to_string();
    let parse = |s: &str| {
        NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|_| anyhow::anyhow!("tanggal tidak valid: {s}"))
    };
    let sp = params(&cfg);

    if p.daily {
        let report_date = p.date.clone().unwrap_or_else(|| ymd(yesterday));
        let day = parse(&report_date)?;
        let data = match collect_daily_summary(&state.db, p.company_id, &report_date, &sp).await? {
            Collected::NoMembers => return Ok(fail("No members found for this company")),
            Collected::Data(d) => d,
        };
        let f = day.format("%a, %-d %b %Y").to_string();
        let ctx = mail_context(
            &cfg.company_name,
            &[("reportDateFormatted", &f), ("subjectDate", &f)],
            &data,
        );
        if !send_manager_summary(state, SummaryKind::Daily, &emails, &ctx).await {
            tracing::warn!("manager-summary/test: daily email not accepted");
        }
        return Ok(
            json!({ "success": true, "message": format!("Daily summary test sent for {report_date}") }),
        );
    }

    let end_date = p.end_date.clone().unwrap_or_else(|| ymd(yesterday));
    let end = parse(&end_date)?;
    let start_date = match &p.start_date {
        Some(s) => s.clone(),
        None => ymd(end.checked_sub_days(Days::new(6)).unwrap_or(end)),
    };
    let start = parse(&start_date)?;
    let data =
        match collect_weekly_summary(&state.db, p.company_id, &start_date, &end_date, &sp).await? {
            Collected::NoMembers => return Ok(fail("No members found for this company")),
            Collected::Data(d) => d,
        };
    let (sf, ef) = (
        start.format("%-d %b").to_string(),
        end.format("%-d %b %Y").to_string(),
    );
    let ctx = mail_context(
        &cfg.company_name,
        &[("startDateFormatted", &sf), ("endDateFormatted", &ef)],
        &data,
    );
    if !send_manager_summary(state, SummaryKind::Weekly, &emails, &ctx).await {
        tracing::warn!("manager-summary/test: weekly email not accepted");
    }
    Ok(
        json!({ "success": true, "message": format!("Weekly summary test sent for {start_date} - {end_date}") }),
    )
}

/// Node: src/admin/admin.controller.ts:331 (`testManagerSummaryEmail`).
pub async fn manager_summary_test(
    State(state): State<AppState>,
    op: Operator,
    audit: AuditCtx,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let p = validate_summary_test(&body)?;
    let res = send_manager_summary_test(&state, &p).await?;
    if res.get("success") != Some(&Value::Bool(false)) {
        audit_after_success(
            &state,
            &op,
            &audit,
            AuditMeta {
                action: "manager_summary.test",
                target_type: "company",
                company_from: Some(CompanyFrom::Body),
            },
            None,
            Some(&body),
        )
        .await;
    }
    Ok((StatusCode::CREATED, Json(res)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msgs(r: Result<impl std::fmt::Debug, ApiError>) -> Vec<String> {
        match r {
            Err(ApiError::Validation(m)) => m,
            other => vec![format!("{other:?}")],
        }
    }

    #[test]
    fn sync_trigger_validation() {
        assert!(validate_sync_trigger(&json!({"date":"2026-11-10"})).is_ok());
        assert_eq!(
            msgs(validate_sync_trigger(&json!({}))),
            [
                "date must be a valid ISO 8601 date string",
                "date must be YYYY-MM-DD"
            ]
        );
        assert_eq!(
            msgs(validate_sync_trigger(&json!({"date":"2026-02-30"}))),
            ["date must be a valid ISO 8601 date string"]
        );
        assert_eq!(
            msgs(validate_sync_trigger(&json!({"date":"2026-11-10","x":1}))),
            ["property x should not exist"]
        );
        assert_eq!(
            msgs(validate_sync_trigger(&json!({"date":"10-11-2026"}))),
            [
                "date must be a valid ISO 8601 date string",
                "date must be YYYY-MM-DD"
            ]
        );
    }

    #[test]
    fn summary_validation() {
        let ok =
            validate_summary_test(&json!({"companyId":"3","type":"weekly","emails":["a@b.co"]}));
        assert_eq!(
            ok.ok(),
            Some(SummaryTest {
                company_id: 3,
                daily: false,
                date: None,
                start_date: None,
                end_date: None,
                emails: Some(vec!["a@b.co".into()]),
            })
        );
        assert_eq!(
            msgs(validate_summary_test(
                &json!({"companyId":0,"type":"x","date":"bad","emails":["nope"]})
            )),
            [
                "companyId must not be less than 1",
                "type must be one of the following values: daily, weekly",
                "date must be YYYY-MM-DD",
                "each value in emails must be an email",
            ]
        );
        assert_eq!(
            msgs(validate_summary_test(
                &json!({"type":"daily","emails":"a@b.co"})
            )),
            [
                "companyId must not be less than 1",
                "companyId must be an integer number",
                "emails must contain no more than 20 elements",
                "emails must be an array",
            ]
        );
    }
}
