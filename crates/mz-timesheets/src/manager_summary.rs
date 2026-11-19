//! Port `src/manager-summary/manager-summary.{controller,service}.ts` (4 route CRUD).
//! Pengumpulan data harian/mingguan + email (`sendManagerSummaryDaily/Weekly`)
//! hanya dipanggil cron Node, jadi TIDAK diport (lihat deviasi).

use axum::Json;
use axum::extract::State;
use http::StatusCode;
use mz_core::company::find_company_id;
use mz_core::extract::ValidatedJson;
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::dto::{ManagerConfigBody, ManagerCreateBody, ManagerUpdateBody, provided};
use crate::rowjson::{MANAGER_SUMMARY_KEYS, fix_dates, order};

const DATE_KEYS: &[&str] = &[
    "createdAt",
    "updatedAt",
    "lastDailySentAt",
    "lastWeeklySentAt",
];

async fn company_id(state: &AppState, user_id: i32) -> ApiResult<i32> {
    find_company_id(&state.db, user_id)
        .await?
        .ok_or_else(|| ApiError::not_found("User company not found"))
}

fn finish(mut row: Value) -> Value {
    // `to_jsonb(row)` mengurutkan kunci menurut panjang lalu abjad; Prisma = urutan field model.
    order(&mut row, MANAGER_SUMMARY_KEYS);
    fix_dates(&mut row, DATE_KEYS);
    row
}

async fn find_config(state: &AppState, company_id: i32) -> ApiResult<Option<Value>> {
    let row: Option<Value> = sqlx::query_scalar(
        r#"SELECT to_jsonb(m) FROM "managerSummaryConfig" m WHERE m."companyId" = $1"#,
    )
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    Ok(row.map(finish))
}

/// Prisma: `null` untuk kolom non-nullable = galat validasi (500).
fn not_null<T>(v: &Value, f: impl Fn(&Value) -> Option<T>) -> ApiResult<T> {
    f(v).ok_or_else(|| ApiError::internal(anyhow::anyhow!("Argument must not be null")))
}

/// Node: src/manager-summary/manager-summary.service.ts:42 (`update`)
async fn update_config(
    state: &AppState,
    user_id: i32,
    dto: &ManagerConfigBody,
) -> ApiResult<Value> {
    let company_id = company_id(state, user_id).await?;
    let Some(id) = sqlx::query_scalar::<_, i32>(
        r#"SELECT id FROM "managerSummaryConfig" WHERE "companyId" = $1"#,
    )
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?
    else {
        return Err(ApiError::not_found("Config not found"));
    };

    let mut qb =
        QueryBuilder::<Postgres>::new(r#"UPDATE "managerSummaryConfig" SET "updatedAt" = "#);
    qb.push_bind(now_naive());
    if let Some(v) = &dto.is_daily_enabled {
        qb.push(r#", "isDailyEnabled" = "#)
            .push_bind(not_null(v, Value::as_bool)?);
    }
    if let Some(v) = &dto.is_weekly_enabled {
        qb.push(r#", "isWeeklyEnabled" = "#)
            .push_bind(not_null(v, Value::as_bool)?);
    }
    if let Some(v) = &dto.send_time {
        qb.push(r#", "sendTime" = "#)
            .push_bind(not_null(v, |x| x.as_str().map(str::to_owned))?);
    }
    if let Some(v) = &dto.include_weekends {
        qb.push(r#", "includeWeekends" = "#)
            .push_bind(not_null(v, Value::as_bool)?);
    }
    if let Some(v) = &dto.emails {
        if v.is_null() {
            return Err(ApiError::internal(anyhow::anyhow!(
                "Argument emails must not be null"
            )));
        }
        qb.push(", emails = ").push_bind(v.clone());
    }
    if let Some(v) = &dto.user_ids {
        qb.push(r#", "userIds" = "#)
            .push_bind((!v.is_null()).then(|| v.clone()));
    }
    if let Some(v) = &dto.low_hours_threshold {
        let n = not_null(v, |x| x.as_i64().and_then(|n| i32::try_from(n).ok()))?;
        qb.push(r#", "lowHoursThreshold" = "#).push_bind(n);
    }
    if let Some(v) = &dto.disabled_flags {
        qb.push(r#", "disabledFlags" = "#)
            .push_bind((!v.is_null()).then(|| v.clone()));
    }
    qb.push(" WHERE id = ").push_bind(id);
    qb.push(r#" RETURNING to_jsonb("managerSummaryConfig".*)"#);
    let row: Value = qb.build_query_scalar().fetch_one(&state.db).await?;
    Ok(json!({ "success": true, "message": "Config updated", "data": finish(row) }))
}

/// Node: src/manager-summary/manager-summary.controller.ts:30 (`create`, default 201)
pub async fn create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(ManagerCreateBody(dto)): ValidatedJson<ManagerCreateBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let user_id = ctx.require_user_id()?;
    let company_id = company_id(&state, user_id).await?;
    let exists: Option<i32> =
        sqlx::query_scalar(r#"SELECT id FROM "managerSummaryConfig" WHERE "companyId" = $1"#)
            .bind(company_id)
            .fetch_optional(&state.db)
            .await?;
    if exists.is_some() {
        return Ok((
            StatusCode::CREATED,
            Json(update_config(&state, user_id, &dto).await?),
        ));
    }

    let flag =
        |v: &Option<Value>, default: bool| provided(v).and_then(Value::as_bool).unwrap_or(default);
    let send_time = provided(&dto.send_time)
        .and_then(Value::as_str)
        .unwrap_or("08:00")
        .to_owned();
    let threshold = match provided(&dto.low_hours_threshold) {
        Some(v) => not_null(v, |x| x.as_i64().and_then(|n| i32::try_from(n).ok()))?,
        None => 14400,
    };
    let row: Value = sqlx::query_scalar(
        r#"INSERT INTO "managerSummaryConfig" ("companyId", "isDailyEnabled", "isWeeklyEnabled", "sendTime",
               "includeWeekends", emails, "userIds", "lowHoursThreshold", "disabledFlags", "createdById")
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
           RETURNING to_jsonb("managerSummaryConfig".*)"#,
    )
    .bind(company_id)
    .bind(flag(&dto.is_daily_enabled, true))
    .bind(flag(&dto.is_weekly_enabled, true))
    .bind(send_time)
    .bind(flag(&dto.include_weekends, false))
    .bind(dto.emails.clone().unwrap_or_else(|| json!([])))
    .bind(provided(&dto.user_ids).cloned())
    .bind(threshold)
    .bind(provided(&dto.disabled_flags).cloned())
    .bind(user_id)
    .fetch_one(&state.db)
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(
            json!({ "success": true, "message": "Manager summary config created", "data": finish(row) }),
        ),
    ))
}

/// Node: src/manager-summary/manager-summary.controller.ts:36 (`findOne`)
pub async fn find_one(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let company_id = company_id(&state, user_id).await?;
    let config = find_config(&state, company_id).await?;
    Ok(Json(json!({ "success": true, "data": config })))
}

/// Node: src/manager-summary/manager-summary.controller.ts:42 (`update`)
pub async fn update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(ManagerUpdateBody(dto)): ValidatedJson<ManagerUpdateBody>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(update_config(&state, user_id, &dto).await?))
}

/// Node: src/manager-summary/manager-summary.controller.ts:48 (`remove`)
pub async fn remove(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let company_id = company_id(&state, user_id).await?;
    let res = sqlx::query(r#"DELETE FROM "managerSummaryConfig" WHERE "companyId" = $1"#)
        .bind(company_id)
        .execute(&state.db)
        .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::not_found("Config not found"));
    }
    Ok(Json(
        json!({ "success": true, "message": "Config deleted" }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_fixes_dates() {
        let v = finish(
            json!({ "createdAt": "2026-11-16T08:00:00", "lastDailySentAt": null, "emails": ["a@b.co"] }),
        );
        assert_eq!(v["createdAt"], "2026-11-16T08:00:00.000Z");
        assert!(v["lastDailySentAt"].is_null());
    }

    #[test]
    fn finish_orders_keys_like_prisma() {
        let v = finish(json!({
            "id": 1, "uuid": "u", "companyId": 2, "isDailyEnabled": true, "emails": [],
            "sendTime": "08:00", "createdById": 3, "createdAt": "2026-11-16T08:00:00"
        }));
        let keys: Vec<&str> = v
            .as_object()
            .map_or(vec![], |m| m.keys().map(String::as_str).collect());
        assert_eq!(
            keys,
            [
                "id",
                "uuid",
                "companyId",
                "isDailyEnabled",
                "sendTime",
                "emails",
                "createdById",
                "createdAt"
            ]
        );
    }

    #[test]
    fn null_rules() {
        assert!(not_null(&Value::Null, Value::as_bool).is_err());
        assert_eq!(not_null(&json!(true), Value::as_bool).ok(), Some(true));
    }
}
