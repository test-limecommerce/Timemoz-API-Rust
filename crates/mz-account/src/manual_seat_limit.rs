//! Port `src/users/manual-seat-limit.ts` — batas kursi company berlangganan MANUAL.
//!
//! WAJIB dipanggil di dalam transaksi yang sama dengan penulisan `userCompany`
//! (lock advisory `seats:<companyId>` dilepas saat commit).

use axum::http::StatusCode;
use mz_core::db::advisory_xact_lock;
use mz_core::{ApiError, ApiResult};
use serde_json::json;
use sqlx::PgConnection;

pub const SEAT_LIMIT_REACHED: &str = "SEAT_LIMIT_REACHED";

/// `assertManualSeatAvailable(tx, companyId, requested, { excludeUserId })`.
/// `used` = `userCompany.status IN (1, 2)`. Gagal: 409
/// `{code, maxUsers, used, message: "Seat limit reached (used/max). Contact the Timemoz team to add seats."}`.
pub async fn assert_manual_seat_available(
    conn: &mut PgConnection,
    company_id: i32,
    requested: i64,
    exclude_user_id: Option<i32>,
) -> ApiResult<()> {
    advisory_xact_lock(conn, &format!("seats:{company_id}")).await?;

    // findCurrentSubscription: startDate desc, id desc, tanpa filter status.
    let billing_source: Option<String> = sqlx::query_scalar(
        r#"SELECT "billingSource"::text FROM subscriptions
           WHERE "companyId" = $1 ORDER BY "startDate" DESC, id DESC LIMIT 1"#,
    )
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    if billing_source.as_deref() != Some("MANUAL") {
        return Ok(());
    }

    let max_users: Option<i32> =
        sqlx::query_scalar(r#"SELECT "maxUsers" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(&mut *conn)
            .await?;
    let max_users = max_users.unwrap_or(0);
    if max_users <= 0 {
        return Ok(());
    }

    let used: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "userCompany"
           WHERE "companyId" = $1 AND status IN (1, 2)
             AND ($2::int IS NULL OR "userId" <> $2)"#,
    )
    .bind(company_id)
    .bind(exclude_user_id)
    .fetch_one(&mut *conn)
    .await?;

    if used + requested > i64::from(max_users) {
        return Err(seat_limit_error(used, max_users));
    }
    Ok(())
}

fn seat_limit_error(used: i64, max_users: i32) -> ApiError {
    ApiError::body(
        StatusCode::CONFLICT,
        json!({
            "code": SEAT_LIMIT_REACHED,
            "maxUsers": max_users,
            "used": used,
            "message": format!("Seat limit reached ({used}/{max_users}). Contact the Timemoz team to add seats."),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_shape() {
        let ApiError::Body { status, body } = seat_limit_error(5, 5) else {
            return assert_eq!(0, 1, "bukan Body");
        };
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body,
            json!({"code":"SEAT_LIMIT_REACHED","maxUsers":5,"used":5,
                   "message":"Seat limit reached (5/5). Contact the Timemoz team to add seats."})
        );
    }
}
