//! `UsersService.getProductTour` (:2809) dan `updateProductTour` (:2834).

use mz_core::time::JsDate;
use mz_core::{ApiResult, AppState};
use serde_json::{Value, json};

#[derive(sqlx::FromRow)]
struct Tour {
    status: Option<String>,
    step: Option<String>,
    #[sqlx(rename = "updatedAt")]
    updated_at: Option<JsDate>,
}

const TERMINAL_STATUSES: [&str; 2] = ["COMPLETED", "SKIPPED"];

fn not_found() -> Value {
    json!({ "success": false, "message": "User not found", "data": null })
}

async fn load(state: &AppState, user_id: i32) -> ApiResult<Option<Tour>> {
    Ok(sqlx::query_as::<_, Tour>(
        r#"SELECT "productTourStatus" AS status, "productTourStep" AS step,
                  "productTourUpdatedAt" AS "updatedAt" FROM users WHERE id = $1"#,
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?)
}

/// `GET /users/me/product-tour`.
pub async fn get_product_tour(state: &AppState, user_id: i32) -> ApiResult<Value> {
    let Some(tour) = load(state, user_id).await? else {
        return Ok(not_found());
    };
    Ok(json!({
        "success": true,
        "message": "success get product tour",
        "data": {
            "status": tour.status.unwrap_or_else(|| "NOT_STARTED".to_owned()),
            "step": tour.step,
            "updatedAt": tour.updated_at,
        },
    }))
}

/// Murni: langkah yang disimpan. Status akhir (`COMPLETED`/`SKIPPED`) menghapus langkah;
/// `IN_PROGRESS` memakai `dto.step ?? step lama ?? null`.
pub fn next_step(status: &str, dto_step: Option<&str>, current: Option<&str>) -> Option<String> {
    if status != "IN_PROGRESS" {
        return None;
    }
    dto_step.or(current).map(str::to_owned)
}

/// `PATCH /users/me/product-tour`.
pub async fn update_product_tour(
    state: &AppState,
    audit: &mz_audit::AuditHandle,
    user_id: i32,
    status: &str,
    step: Option<&str>,
) -> ApiResult<Value> {
    let Some(tour) = load(state, user_id).await? else {
        return Ok(not_found());
    };

    // Terminal lock: sesudah COMPLETED/SKIPPED tour tidak pernah dibuka lagi (dijawab 200 idempoten).
    if tour
        .status
        .as_deref()
        .is_some_and(|s| TERMINAL_STATUSES.contains(&s))
    {
        return Ok(json!({
            "success": true,
            "message": "product tour already finalized",
            "data": { "status": tour.status, "step": null, "updatedAt": tour.updated_at },
        }));
    }

    let new_step = next_step(status, step, tour.step.as_deref());
    // Audit `users.update` ber-`select` tiga kolom productTour*: hasilnya tanpa `id`, jadi Node
    // tidak pernah melahirkan baris (entityId null) — pra-bacanya tetap terjadi, ditiru apa adanya.
    let pre = audit
        .pre_read(mz_audit::Model::Users, mz_audit::Lookup::Id(user_id))
        .await;
    let updated = sqlx::query_as::<_, Tour>(
        r#"UPDATE users SET "productTourStatus" = $2, "productTourStep" = $3, "productTourUpdatedAt" = $4
           WHERE id = $1
           RETURNING "productTourStatus" AS status, "productTourStep" AS step,
                     "productTourUpdatedAt" AS "updatedAt""#,
    )
    .bind(user_id)
    .bind(status)
    .bind(new_step)
    .bind(JsDate::now())
    .fetch_one(&state.db)
    .await?;
    audit
        .record_update(
            mz_audit::Model::Users,
            pre,
            &mz_audit::Row::new()
                .with("productTourStatus", updated.status.clone())
                .with("productTourStep", updated.step.clone())
                .with("productTourUpdatedAt", json!(updated.updated_at)),
        )
        .await;
    Ok(json!({
        "success": true,
        "message": "success update product tour",
        "data": { "status": updated.status, "step": updated.step, "updatedAt": updated.updated_at },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_status_clears_step() {
        assert_eq!(next_step("COMPLETED", Some("report-menu"), Some("x")), None);
        assert_eq!(next_step("SKIPPED", None, Some("x")), None);
    }

    #[test]
    fn in_progress_prefers_dto_then_current() {
        assert_eq!(
            next_step("IN_PROGRESS", Some("a"), Some("b")).as_deref(),
            Some("a")
        );
        assert_eq!(
            next_step("IN_PROGRESS", None, Some("b")).as_deref(),
            Some("b")
        );
        assert_eq!(next_step("IN_PROGRESS", None, None), None);
    }
}
