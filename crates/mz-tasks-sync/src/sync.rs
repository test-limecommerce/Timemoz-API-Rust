//! `TasksService.runSyncForDate` (`tasks/tasks.service.ts:126`).

use chrono::NaiveDate;
use serde_json::{Value, json};
use sqlx::PgPool;

/// Hasil `runSyncForDate`: `{ message, date }`.
pub fn outcome_json(has_error: bool, date: &str) -> Value {
    json!({ "message": if has_error { "Partial success" } else { "Success" }, "date": date })
}

/// Node: `tasks.service.ts:126` `runSyncForDate`. `date` = `YYYY-MM-DD`
/// (`date.toISOString().split('T')[0]`). Dua statement atomik terpisah ("Partial success").
pub async fn run_sync_for_date(db: &PgPool, date: NaiveDate) -> Value {
    let date_string = date.format("%Y-%m-%d").to_string();
    tracing::info!("Memulai sinkronisasi untuk: {date_string}");
    let mut has_error = false;

    // 1. Level user-project (companyId dari userCompany default).
    match sqlx::query(
        r#"INSERT INTO "dailyUserProjectSummary" ("reportDate", "userId", "projectId", "companyId", "totalSeconds")
           SELECT
             $1::date,
             "userId", "projectId", "companyId",
             SUM(seconds)::int
           FROM (
             SELECT DISTINCT ON (tt.id)
               t."userId",
               tt."projectId",
               uc."companyId",
               EXTRACT(EPOCH FROM (tt."endAt" - tt."startAt"))::int as seconds
             FROM "trackingTimeline" tt
             JOIN "tracking" t ON tt."trackingId" = t.id
             JOIN "userCompany" uc ON t."userId" = uc."userId" AND uc."status" = 1 AND uc."isDefault" = true
             WHERE t."takenDate" = $1::date
               AND tt.status = 1
           ) AS unique_timeline
           GROUP BY "userId", "projectId", "companyId"
           ORDER BY "userId", "projectId", "companyId"
           ON CONFLICT ("userId", "projectId", "reportDate")
           DO UPDATE SET "totalSeconds" = EXCLUDED."totalSeconds", "companyId" = EXCLUDED."companyId""#,
    )
    .bind(date)
    .execute(db)
    .await
    {
        Ok(r) => tracing::info!("[Sync {date_string}] User-project: {} rows", r.rows_affected()),
        Err(err) => {
            tracing::error!("[Sync {date_string}] User-project aggregation failed: {err}");
            has_error = true;
        }
    }

    // 2+3. Level project & company dalam satu statement (CTE dipakai dua kali).
    match sqlx::query_as::<_, (i64, i64)>(
        r#"WITH unique_timeline AS (
             SELECT DISTINCT ON (tt.id)
               tt."projectId",
               pc."companyId",
               EXTRACT(EPOCH FROM (tt."endAt" - tt."startAt"))::int as seconds
             FROM "trackingTimeline" tt
             JOIN "tracking" t ON tt."trackingId" = t.id
             JOIN "projectCompany" pc ON tt."projectId" = pc."projectId"
             WHERE t."takenDate" = $1::date
               AND tt.status = 1
           ),
           project_upsert AS (
             INSERT INTO "dailyProjectSummary" ("reportDate", "projectId", "companyId", "totalSeconds")
             SELECT $1::date, "projectId", "companyId", SUM(seconds)::int
             FROM unique_timeline
             GROUP BY "projectId", "companyId"
             ORDER BY "projectId", "companyId"
             ON CONFLICT ("projectId", "companyId", "reportDate")
             DO UPDATE SET "totalSeconds" = EXCLUDED."totalSeconds"
             RETURNING 1
           ),
           company_upsert AS (
             INSERT INTO "dailyCompanySummary" ("reportDate", "companyId", "totalSeconds")
             SELECT $1::date, "companyId", SUM(seconds)::int
             FROM unique_timeline
             GROUP BY "companyId"
             ORDER BY "companyId"
             ON CONFLICT ("companyId", "reportDate")
             DO UPDATE SET "totalSeconds" = EXCLUDED."totalSeconds"
             RETURNING 1
           )
           SELECT
             (SELECT COUNT(*) FROM project_upsert) AS "projectRows",
             (SELECT COUNT(*) FROM company_upsert) AS "companyRows""#,
    )
    .bind(date)
    .fetch_one(db)
    .await
    {
        Ok((p, c)) => {
            tracing::info!("[Sync {date_string}] Project: {p} rows");
            tracing::info!("[Sync {date_string}] Company: {c} rows");
        }
        Err(err) => {
            tracing::error!("[Sync {date_string}] Project/company aggregation failed: {err}");
            has_error = true;
        }
    }

    if has_error {
        tracing::warn!("Sync selesai dengan error untuk {date_string}");
    } else {
        tracing::info!("Sync sukses untuk {date_string}");
    }
    outcome_json(has_error, &date_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_shape() {
        assert_eq!(
            outcome_json(false, "2026-11-10"),
            json!({"message":"Success","date":"2026-11-10"})
        );
        assert_eq!(
            outcome_json(true, "2026-11-10"),
            json!({"message":"Partial success","date":"2026-11-10"})
        );
    }
}
