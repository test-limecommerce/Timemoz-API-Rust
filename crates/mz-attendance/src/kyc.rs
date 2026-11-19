//! Port `src/mobile-kyc/mobile-kyc.service.ts`.

use mz_core::time::now_naive;
use mz_core::{ApiResult, AppState};
use serde_json::{Value, json};

/// Node: `saveFace` — upsert `userKycFace` per user.
pub async fn save_face(state: &AppState, user_id: i32, faces: &str) -> ApiResult<Value> {
    let faces: String = sqlx::query_scalar(
        r#"INSERT INTO "userKycFace" ("userId", faces) VALUES ($1, $2)
           ON CONFLICT ("userId") DO UPDATE SET faces = EXCLUDED.faces, "updatedAt" = $3
           RETURNING faces"#,
    )
    .bind(user_id)
    .bind(faces)
    .bind(now_naive())
    .fetch_one(&state.db)
    .await?;
    Ok(json!({ "faces": faces }))
}

/// Node: `getFace`.
pub async fn get_face(state: &AppState, user_id: i32) -> ApiResult<Value> {
    let faces: Option<String> =
        sqlx::query_scalar(r#"SELECT faces FROM "userKycFace" WHERE "userId" = $1"#)
            .bind(user_id)
            .fetch_optional(&state.db)
            .await?;
    Ok(json!({ "faces": faces }))
}
