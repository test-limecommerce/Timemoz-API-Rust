//! `UsersService.upsertLocalDb` (:2446) dan `getLocalDb` (:2456).

use chrono::NaiveDate;
use mz_core::time::{JsDate, parse_js_date};
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Value, json};

/// `new Date(date)` untuk kolom `@db.Date`: tanggal UTC-nya. `Invalid Date` -> Prisma melempar -> 500.
pub fn db_date(raw: Option<&str>) -> ApiResult<NaiveDate> {
    raw.and_then(parse_js_date)
        .map(|d| d.date())
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("Invalid Date")))
}

#[derive(sqlx::FromRow)]
struct LocalDbRow {
    id: i32,
    uuid: String,
    #[sqlx(rename = "userId")]
    user_id: i32,
    date: JsDate,
    data: Value,
    #[sqlx(rename = "createdAt")]
    created_at: JsDate,
    #[sqlx(rename = "updatedAt")]
    updated_at: Option<JsDate>,
}

const COLUMNS: &str = r#"id, uuid::text AS "uuid", "userId", date::timestamp AS date, data, "createdAt", "updatedAt""#;

fn record_json(r: &LocalDbRow) -> Value {
    json!({
        "id": r.id,
        "uuid": r.uuid,
        "userId": r.user_id,
        "date": r.date,
        "data": r.data,
        "createdAt": r.created_at,
        "updatedAt": r.updated_at,
    })
}

/// `POST /users/local-db` — upsert pada unique (`userId`, `date`).
pub async fn upsert_local_db(
    state: &AppState,
    user_id: i32,
    date: &str,
    data: Value,
) -> ApiResult<Value> {
    let date = db_date(Some(date))?;
    let row = sqlx::query_as::<_, LocalDbRow>(&format!(
        r#"INSERT INTO "userLocalDb" ("userId", date, data) VALUES ($1, $2, $3)
           ON CONFLICT ("userId", date) DO UPDATE SET data = EXCLUDED.data, "updatedAt" = $4
           RETURNING {COLUMNS}"#
    ))
    .bind(user_id)
    .bind(date)
    .bind(data)
    .bind(JsDate::now())
    .fetch_one(&state.db)
    .await?;
    Ok(json!({ "success": true, "message": "Local DB saved", "data": record_json(&row) }))
}

/// `GET /users/local-db?date=` — `data: null` bila tidak ada.
pub async fn get_local_db(state: &AppState, user_id: i32, date: Option<&str>) -> ApiResult<Value> {
    let date = db_date(date)?;
    let row = sqlx::query_as::<_, LocalDbRow>(&format!(
        r#"SELECT {COLUMNS} FROM "userLocalDb" WHERE "userId" = $1 AND date = $2"#
    ))
    .bind(user_id)
    .bind(date)
    .fetch_optional(&state.db)
    .await?;
    Ok(json!({ "success": true, "data": row.as_ref().map(record_json) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_is_utc_day() {
        assert_eq!(
            db_date(Some("2024-01-15"))
                .ok()
                .map(|d| d.to_string())
                .as_deref(),
            Some("2024-01-15")
        );
        assert_eq!(
            db_date(Some("2024-01-15T23:00:00-05:00"))
                .ok()
                .map(|d| d.to_string())
                .as_deref(),
            Some("2024-01-16")
        );
    }

    #[test]
    fn invalid_or_missing_date_is_internal_error() {
        assert!(db_date(None).is_err());
        assert!(db_date(Some("not a date")).is_err());
    }
}
