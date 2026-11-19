//! Helper bersama: format tanggal Prisma/JS, pola respons `{success,message,data}`,
//! `ParseUUIDPipe`, dan query peran (`roleCodeFilter`).

use chrono::{NaiveDate, NaiveDateTime};
use mz_core::ApiError;
use mz_core::time::JsDate;
pub use mz_core::uuid_rules::uuid_pipe as parse_uuid;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::jsfmt;

/// `Date` kolom `timestamp(3)` → `2026-11-16T08:00:00.000Z`.
pub fn jd(dt: NaiveDateTime) -> Value {
    Value::String(JsDate(dt).to_js_string())
}

pub fn jd_opt(dt: Option<NaiveDateTime>) -> Value {
    dt.map_or(Value::Null, jd)
}

/// `toDateString(date)`: `YYYY-MM-DD` dari kolom `@db.Date`.
pub fn date_str(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// Kolom `@db.Date` serialisasi JSON langsung (`Date.toJSON()`).
pub fn date_json(d: NaiveDate) -> Value {
    Value::String(format!("{}T00:00:00.000Z", date_str(d)))
}

/// Angka Float Prisma → JSON seperti `JSON.stringify`.
pub fn fnum(x: Option<f64>) -> Value {
    x.map_or(Value::Null, jsfmt::num)
}

/// `{ success: false, message, data: null }` (HTTP status tetap 200/201).
pub fn fail(message: &str) -> Value {
    json!({ "success": false, "message": message, "data": null })
}

/// `res.status(code).json({ success: false, message })` pada ekspor.
pub fn plain_fail(status: http::StatusCode, message: &str) -> ApiError {
    ApiError::body(status, json!({ "success": false, "message": message }))
}

const COMPANY_ROLES: &[&str] = &["SUPER_ADMIN", "ADMIN", "MANAGER"];
const COMPANY_BASE_ROLES: &[&str] = &["ADMIN", "MANAGER"];

/// `userAccessLevel.findFirst({status 1, accessLevel: roleCodeFilter([SYSTEM_ADMIN])})`.
/// `roleCodeFilter` hanya menerima baris `isSystem` ber-code `SYSTEM_ADMIN`.
pub async fn is_system_admin(db: &PgPool, user_id: i32) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        r#"SELECT EXISTS(
             SELECT 1 FROM "userAccessLevel" ual
             JOIN "accessLevel" al ON al.id = ual."accessLevelId"
             WHERE ual."userId" = $1 AND ual.status = 1
               AND al."isSystem" AND al.code = 'SYSTEM_ADMIN')"#,
    )
    .bind(user_id)
    .fetch_one(db)
    .await
}

/// SUPER_ADMIN / ADMIN / MANAGER di company tertentu, termasuk role custom
/// yang `baseRole`-nya ADMIN / MANAGER (`roleCodeFilter`).
pub async fn is_company_approver(db: &PgPool, user_id: i32, company_id: i32) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        r#"SELECT EXISTS(
             SELECT 1 FROM "userAccessLevel" ual
             JOIN "accessLevel" al ON al.id = ual."accessLevelId"
             WHERE ual."userId" = $1 AND ual."companyId" = $2 AND ual.status = 1
               AND ((al."isSystem" AND al.code = ANY($3))
                    OR (NOT al."isSystem" AND al."baseRole"::text = ANY($4))))"#,
    )
    .bind(user_id)
    .bind(company_id)
    .bind(COMPANY_ROLES)
    .bind(COMPANY_BASE_ROLES)
    .fetch_one(db)
    .await
}

/// `canActOnTimesheet` / `assertPrivileged`.
pub async fn can_act_on_company(db: &PgPool, user_id: i32, company_id: i32) -> sqlx::Result<bool> {
    if is_system_admin(db, user_id).await? {
        return Ok(true);
    }
    is_company_approver(db, user_id, company_id).await
}

/// Calon approver (nama + email) untuk satu company, digabung dengan SYSTEM_ADMIN
/// global dan dideduplikasi per user id (urutan penyisipan dipertahankan).
pub async fn approvers(db: &PgPool, company_id: i32) -> sqlx::Result<Vec<(i32, String, String)>> {
    let company: Vec<(i32, String, String)> = sqlx::query_as(
        r#"SELECT u.id, u.name, u.username FROM "userAccessLevel" ual
           JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           JOIN users u ON u.id = ual."userId"
           WHERE ual."companyId" = $1 AND ual.status = 1
             AND ((al."isSystem" AND al.code = ANY($2))
                  OR (NOT al."isSystem" AND al."baseRole"::text = ANY($3)))
           ORDER BY ual.id"#,
    )
    .bind(company_id)
    .bind(COMPANY_ROLES)
    .bind(COMPANY_BASE_ROLES)
    .fetch_all(db)
    .await?;
    let system: Vec<(i32, String, String)> = sqlx::query_as(
        r#"SELECT u.id, u.name, u.username FROM "userAccessLevel" ual
           JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           JOIN users u ON u.id = ual."userId"
           WHERE ual.status = 1 AND al."isSystem" AND al.code = 'SYSTEM_ADMIN'
           ORDER BY ual.id"#,
    )
    .fetch_all(db)
    .await?;
    Ok(dedupe_approvers(company.into_iter().chain(system)))
}

/// Deduplikasi berdasarkan id user, pertama menang.
pub fn dedupe_approvers(
    it: impl IntoIterator<Item = (i32, String, String)>,
) -> Vec<(i32, String, String)> {
    let mut out: Vec<(i32, String, String)> = Vec::new();
    for row in it {
        if !out.iter().any(|(id, _, _)| *id == row.0) {
            out.push(row);
        }
    }
    out
}

/// Baris timeline yang dihitung sebagai jam kerja (`status 1`, online atau
/// manual-time APPROVED) milik satu user dalam rentang `takenDate`.
pub const TIMELINE_FILTER: &str = r#"t."userId" = $1 AND t."takenDate" >= $2 AND t."takenDate" <= $3
    AND tl.status = 1 AND (tl."isOffline" = false OR (tl."isOffline" = true AND tl."manualTimeStatus" = 'APPROVED'))"#;

/// `calculateTotalSeconds`.
pub async fn total_seconds(
    db: &PgPool,
    user_id: i32,
    start: NaiveDate,
    end: NaiveDate,
) -> sqlx::Result<i64> {
    let rows: Vec<(NaiveDateTime, NaiveDateTime)> = sqlx::query_as(&format!(
        r#"SELECT tl."startAt", tl."endAt" FROM "trackingTimeline" tl
           JOIN tracking t ON t.id = tl."trackingId"
           WHERE {TIMELINE_FILTER}"#
    ))
    .bind(user_id)
    .bind(start)
    .bind(end)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(a, b)| crate::calc::duration_seconds(a, b))
        .sum())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_keeps_first() {
        let rows = vec![
            (1, "A".to_owned(), "a@x".to_owned()),
            (2, "B".to_owned(), "b@x".to_owned()),
            (1, "A2".to_owned(), "a2@x".to_owned()),
        ];
        let out = dedupe_approvers(rows);
        assert_eq!(out.len(), 2);
        assert_eq!(out.first().map(|r| r.1.as_str()), Some("A"));
    }

    #[test]
    fn json_shapes() {
        let d = NaiveDate::from_ymd_opt(2026, 4, 1).unwrap_or_default();
        assert_eq!(date_str(d), "2026-04-01");
        assert_eq!(
            date_json(d),
            Value::String("2026-04-01T00:00:00.000Z".into())
        );
        assert_eq!(fnum(Some(15.0)), json!(15));
        assert_eq!(fnum(None), Value::Null);
        assert_eq!(fail("x")["success"], json!(false));
        assert!(parse_uuid("nope".into()).is_err());
    }
}
