//! Port `src/scheduled-reports/scheduled-reports.{controller,service}.ts` (5 route).

use axum::Json;
use axum::extract::State;
use mz_core::company::find_company_id;
use mz_core::extract::{PathParams, ValidatedJson};
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::common::parse_uuid;
use crate::dto::{ScheduledCreateBody, ScheduledReportBody, ScheduledUpdateBody, provided};
use crate::rowjson::{SCHEDULED_REPORT_KEYS, fix_dates, order};

const DATE_KEYS: &[&str] = &["createdAt", "updatedAt", "lastSentAt"];

/// Satu baris + `createdBy { id, name, username }` (`include`). Baris utuh (`to_jsonb`) disusun
/// ulang menurut field model di [`finish`]; `createdBy` memakai `json_build_object` (tipe `json`
/// menjaga urutan argumen = urutan `select` Node, jsonb tidak) lalu ditempel setelah kolom skalar.
const WITH_CREATOR: &str = r#"to_jsonb(r),
    (SELECT json_build_object('id', u.id, 'name', u.name, 'username', u.username)
     FROM users u WHERE u.id = r."createdById")"#;

async fn company_id(state: &AppState, user_id: i32) -> ApiResult<i32> {
    find_company_id(&state.db, user_id)
        .await?
        .ok_or_else(|| ApiError::not_found("User company not found"))
}

fn finish(mut row: Value) -> Value {
    order(&mut row, SCHEDULED_REPORT_KEYS);
    fix_dates(&mut row, DATE_KEYS);
    if let Some(c) = row.get_mut("createdBy") {
        fix_dates(c, DATE_KEYS);
    }
    row
}

/// Baris `include: { createdBy }`: `createdBy` ditempel setelah kolom skalar (urutan Node).
fn with_creator((mut row, creator): (Value, Option<Value>)) -> Value {
    if let Value::Object(m) = &mut row {
        m.insert("createdBy".to_owned(), creator.unwrap_or(Value::Null));
    }
    finish(row)
}

fn not_found() -> ApiError {
    ApiError::not_found("Scheduled report not found")
}

/// Node: src/scheduled-reports/scheduled-reports.controller.ts:36 (`create`, `@HttpCode(200)`)
pub async fn create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(ScheduledCreateBody(dto)): ValidatedJson<ScheduledCreateBody>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let company_id = company_id(&state, user_id).await?;
    let text = |v: &Option<Value>| {
        provided(v)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let row: Value = sqlx::query_scalar(
        r#"INSERT INTO "scheduledReport" (name, "sendTime", frequency, days, emails, subject, message,
               "userIds", "projectIds", sections, "companyId", "createdById")
           VALUES ($1, $2, $3::"ReportFrequency", $4, $5, $6, $7, $8, $9, $10, $11, $12)
           RETURNING to_jsonb("scheduledReport".*)"#,
    )
    .bind(text(&dto.name))
    .bind(text(&dto.send_time))
    .bind(text(&dto.frequency))
    .bind(provided(&dto.days).cloned())
    .bind(dto.emails.clone().unwrap_or_else(|| json!([])))
    .bind(text(&dto.subject))
    .bind(text(&dto.message))
    .bind(provided(&dto.user_ids).cloned())
    .bind(provided(&dto.project_ids).cloned())
    .bind(provided(&dto.sections).cloned())
    .bind(company_id)
    .bind(user_id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(json!({
        "success": true, "message": "Scheduled report created successfully", "data": finish(row),
    })))
}

/// Node: src/scheduled-reports/scheduled-reports.controller.ts:43 (`findAll`)
pub async fn find_all(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let company_id = company_id(&state, user_id).await?;
    let rows: Vec<(Value, Option<Value>)> = sqlx::query_as(&format!(
        r#"SELECT {WITH_CREATOR} FROM "scheduledReport" r
           WHERE r."companyId" = $1 ORDER BY r."createdAt" DESC, r.id DESC"#
    ))
    .bind(company_id)
    .fetch_all(&state.db)
    .await?;
    let data: Vec<Value> = rows.into_iter().map(with_creator).collect();
    Ok(Json(json!({
        "success": true, "message": "Scheduled reports fetched successfully", "data": data,
    })))
}

/// Node: src/scheduled-reports/scheduled-reports.controller.ts:50 (`findOne`)
pub async fn find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    let user_id = ctx.require_user_id()?;
    let company_id = company_id(&state, user_id).await?;
    let row: Option<(Value, Option<Value>)> = sqlx::query_as(&format!(
        r#"SELECT {WITH_CREATOR} FROM "scheduledReport" r WHERE r.uuid = $1::uuid AND r."companyId" = $2"#
    ))
    .bind(&uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let row = row.ok_or_else(not_found)?;
    Ok(Json(json!({
        "success": true, "message": "Scheduled report fetched successfully", "data": with_creator(row),
    })))
}

/// Prisma: `null` untuk kolom non-nullable = galat validasi (500).
fn text_not_null(v: &Value) -> ApiResult<String> {
    v.as_str()
        .map(str::to_owned)
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("Argument must not be null")))
}

fn push_update_fields(
    qb: &mut QueryBuilder<'_, Postgres>,
    dto: &ScheduledReportBody,
) -> ApiResult<()> {
    if let Some(v) = &dto.name {
        qb.push(", name = ").push_bind(text_not_null(v)?);
    }
    if let Some(v) = &dto.send_time {
        qb.push(r#", "sendTime" = "#).push_bind(text_not_null(v)?);
    }
    if let Some(v) = &dto.frequency {
        qb.push(", frequency = ")
            .push_bind(text_not_null(v)?)
            .push(r#"::"ReportFrequency""#);
    }
    if let Some(v) = &dto.days {
        qb.push(", days = ")
            .push_bind((!v.is_null()).then(|| v.clone()));
    }
    if let Some(v) = &dto.emails {
        if v.is_null() {
            return Err(ApiError::internal(anyhow::anyhow!(
                "Argument emails must not be null"
            )));
        }
        qb.push(", emails = ").push_bind(v.clone());
    }
    if let Some(v) = &dto.subject {
        qb.push(", subject = ").push_bind(text_not_null(v)?);
    }
    if let Some(v) = &dto.message {
        qb.push(", message = ").push_bind(text_not_null(v)?);
    }
    if let Some(v) = &dto.user_ids {
        qb.push(r#", "userIds" = "#)
            .push_bind((!v.is_null()).then(|| v.clone()));
    }
    if let Some(v) = &dto.project_ids {
        qb.push(r#", "projectIds" = "#)
            .push_bind((!v.is_null()).then(|| v.clone()));
    }
    if let Some(v) = &dto.sections {
        qb.push(", sections = ")
            .push_bind((!v.is_null()).then(|| v.clone()));
    }
    if let Some(v) = &dto.is_active {
        let b = v.as_bool().ok_or_else(|| {
            ApiError::internal(anyhow::anyhow!("Argument isActive must not be null"))
        })?;
        qb.push(r#", "isActive" = "#).push_bind(b);
    }
    Ok(())
}

/// Node: src/scheduled-reports/scheduled-reports.controller.ts:57 (`update`)
pub async fn update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
    ValidatedJson(ScheduledUpdateBody(dto)): ValidatedJson<ScheduledUpdateBody>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    let user_id = ctx.require_user_id()?;
    let company_id = company_id(&state, user_id).await?;
    let id: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "scheduledReport" WHERE uuid = $1::uuid AND "companyId" = $2"#,
    )
    .bind(&uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let id = id.ok_or_else(not_found)?;

    let mut qb = QueryBuilder::<Postgres>::new(r#"UPDATE "scheduledReport" SET "updatedAt" = "#);
    qb.push_bind(now_naive());
    push_update_fields(&mut qb, &dto)?;
    qb.push(" WHERE id = ").push_bind(id);
    qb.push(r#" RETURNING to_jsonb("scheduledReport".*)"#);
    let row: Value = qb.build_query_scalar().fetch_one(&state.db).await?;
    Ok(Json(json!({
        "success": true, "message": "Scheduled report updated successfully", "data": finish(row),
    })))
}

/// Node: src/scheduled-reports/scheduled-reports.controller.ts:68 (`remove`)
pub async fn remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid(uuid)?;
    let user_id = ctx.require_user_id()?;
    let company_id = company_id(&state, user_id).await?;
    let res =
        sqlx::query(r#"DELETE FROM "scheduledReport" WHERE uuid = $1::uuid AND "companyId" = $2"#)
            .bind(&uuid)
            .bind(company_id)
            .execute(&state.db)
            .await?;
    if res.rows_affected() == 0 {
        return Err(not_found());
    }
    Ok(Json(
        json!({ "success": true, "message": "Scheduled report deleted successfully" }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_fixes_nested_dates() {
        let v = finish(json!({
            "createdAt": "2026-11-16T08:00:00", "lastSentAt": null,
            "createdBy": { "id": 1, "name": "A", "username": "a@b.co" }
        }));
        assert_eq!(v["createdAt"], "2026-11-16T08:00:00.000Z");
        assert_eq!(v["createdBy"]["name"], "A");
    }

    #[test]
    fn row_keys_follow_prisma_order_with_creator_last() {
        let v = with_creator((
            json!({
                "id": 1, "uuid": "u", "name": "n", "emails": [], "sendTime": "08:00",
                "isActive": true, "companyId": 2, "createdById": 3, "createdAt": "2026-11-16T08:00:00"
            }),
            Some(json!({ "id": 3, "name": "A", "username": "a@b.co" })),
        ));
        let keys: Vec<&str> = v
            .as_object()
            .map_or(vec![], |m| m.keys().map(String::as_str).collect());
        assert_eq!(
            keys,
            [
                "id",
                "uuid",
                "name",
                "sendTime",
                "emails",
                "isActive",
                "companyId",
                "createdById",
                "createdAt",
                "createdBy"
            ]
        );
        let creator: Vec<&str> = v["createdBy"]
            .as_object()
            .map_or(vec![], |m| m.keys().map(String::as_str).collect());
        assert_eq!(creator, ["id", "name", "username"]);
    }

    #[test]
    fn update_builder_skips_absent_and_rejects_null_required() {
        let mut qb = QueryBuilder::<Postgres>::new("UPDATE t SET x = 1");
        let dto = ScheduledReportBody {
            is_active: Some(json!(false)),
            days: Some(Value::Null),
            ..Default::default()
        };
        assert!(push_update_fields(&mut qb, &dto).is_ok());
        let sql = qb.sql().to_string();
        assert!(sql.contains(r#""isActive" = "#) && sql.contains("days = "));
        assert!(!sql.contains("subject"));
        let mut qb = QueryBuilder::<Postgres>::new("UPDATE t SET x = 1");
        let bad = ScheduledReportBody {
            name: Some(Value::Null),
            ..Default::default()
        };
        assert!(push_update_fields(&mut qb, &bad).is_err());
    }
}
