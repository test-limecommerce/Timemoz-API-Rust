//! `POST|GET /users/local-db` (batas body 50 MB) dan `POST|GET /users/log` (10 MB), semuanya
//! `@HttpCode(200)` + `@PermissionExempt('self-service')`.

use axum::Json;
use axum::extract::{Query, State};
use mz_core::extract::ValidatedJson;
use mz_core::{ApiResult, AppState, RequestCtx};
use serde_json::Value;

use crate::dto::{LocalDbBody, UserLogBody};
use crate::service::{local_db, logs};

/// `@Query('date') date: string` — nilai pertama `date`, kalau ada.
fn date_param(pairs: &[(String, String)]) -> Option<&str> {
    pairs
        .iter()
        .find(|(k, _)| k == "date")
        .map(|(_, v)| v.as_str())
}

/// Node: src/users/users.controller.ts:206 (`upsertLocalDb`)
pub async fn upsert_local_db(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(body): ValidatedJson<LocalDbBody>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        local_db::upsert_local_db(&state, user_id, &body.date(), body.data()).await?,
    ))
}

/// Node: src/users/users.controller.ts:220 (`getLocalDb`)
pub async fn get_local_db(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Query(pairs): Query<Vec<(String, String)>>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        local_db::get_local_db(&state, user_id, date_param(&pairs)).await?,
    ))
}

/// Node: src/users/users.controller.ts:232 (`createLog`)
pub async fn create_log(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(body): ValidatedJson<UserLogBody>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        logs::create_log(&state, user_id, &body.date(), &body.content()).await?,
    ))
}

/// Node: src/users/users.controller.ts:246 (`getLogs`)
pub async fn get_logs(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Query(pairs): Query<Vec<(String, String)>>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        logs::get_logs(&state, user_id, date_param(&pairs)).await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_param_takes_first_match() {
        let pairs = vec![
            ("x".to_owned(), "1".to_owned()),
            ("date".to_owned(), "2024-01-15".to_owned()),
            ("date".to_owned(), "2024-01-16".to_owned()),
        ];
        assert_eq!(date_param(&pairs), Some("2024-01-15"));
        assert_eq!(date_param(&[]), None);
    }
}
