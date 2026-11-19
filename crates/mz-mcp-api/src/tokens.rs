//! Port `src/mcp-api/tokens/*`: `POST|GET /mcp-api/tokens`, `DELETE /mcp-api/tokens/:uuid`
//! (`JwtAuthGuard`, `@PermissionExempt('self-scoped')`: seluruhnya terikat user pemanggil).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use mz_admin_mcp::body::{Body, V, str_of};
use mz_core::gate::RequestCtx;
use mz_core::time::{JsDate, now_naive};
use mz_core::uuid_rules::parse_uuid_pipe;
use mz_core::{ApiError, ApiResult, AppState};
use rand::TryRngCore;
use serde_json::{Value, json};

use crate::pat::{PAT_PREFIX, hash_token};

/// `PatService.generate()`: `mcp_pat_` + 16 byte acak (hex); prefix = 12 karakter pertama.
fn generate() -> ApiResult<(String, String, String)> {
    let mut buf = [0_u8; 16];
    rand::rngs::OsRng
        .try_fill_bytes(&mut buf)
        .map_err(ApiError::internal)?;
    let plain = format!("{PAT_PREFIX}{}", hex::encode(buf));
    let hash = hash_token(&plain);
    let prefix: String = plain.chars().take(12).collect();
    Ok((plain, hash, prefix))
}

/// Node: src/mcp-api/tokens/tokens.controller.ts:21 (`POST`, 201)
pub async fn h_create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Body(b): Body,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let user_id = ctx.require_user_id()?;
    // `CreateTokenDto`: @IsString @IsNotEmpty @MaxLength(255) (pipe global).
    let mut v = V::default();
    v.max_length("name", b.get("name"), 255);
    v.not_empty_string("name", b.get("name"));
    v.finish()?;
    let name = str_of(&b, "name").unwrap_or_default();

    let (plain, hash, prefix) = generate()?;
    let (uuid, name, token_prefix): (String, String, String) = sqlx::query_as(
        r#"INSERT INTO "mcpAccessToken" ("userId", name, "tokenHash", "tokenPrefix", "expiresAt", status)
           VALUES ($1, $2, $3, $4, NULL, 1)
           RETURNING uuid::text, name, "tokenPrefix""#,
    )
    .bind(user_id)
    .bind(name)
    .bind(hash)
    .bind(prefix)
    .fetch_one(&state.db)
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "token": plain, "uuid": uuid, "name": name, "prefix": token_prefix })),
    ))
}

type ListRow = (
    String,
    String,
    String,
    Vec<String>,
    Option<JsDate>,
    Option<JsDate>,
    Option<JsDate>,
    JsDate,
);

/// Node: src/mcp-api/tokens/tokens.controller.ts:27 (`GET`)
pub async fn h_list(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let rows: Vec<ListRow> = sqlx::query_as(
        r#"SELECT uuid::text, name, "tokenPrefix", scopes, "lastUsedAt", "expiresAt", "revokedAt", "createdAt"
             FROM "mcpAccessToken"
            WHERE "userId" = $1 AND status = 1
            ORDER BY "createdAt" DESC, id DESC"#,
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await?;
    let data: Vec<Value> = rows
        .into_iter()
        .map(|(uuid, name, prefix, scopes, last, exp, rev, created)| {
            json!({
                "uuid": uuid,
                "name": name,
                "tokenPrefix": prefix,
                "scopes": scopes,
                "lastUsedAt": last,
                "expiresAt": exp,
                "revokedAt": rev,
                "createdAt": created,
            })
        })
        .collect();
    Ok(Json(json!({ "data": data })))
}

/// Node: src/mcp-api/tokens/tokens.controller.ts:33 (`DELETE :uuid`, `ParseUUIDPipe`)
pub async fn h_revoke(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(raw): Path<String>,
) -> ApiResult<Json<Value>> {
    let user_id = ctx.require_user_id()?;
    let uuid = parse_uuid_pipe(&raw)?;
    let id: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "mcpAccessToken"
            WHERE uuid = $1 AND "userId" = $2 AND status = 1 AND "revokedAt" IS NULL
            LIMIT 1"#,
    )
    .bind(uuid)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(id) = id else {
        return Err(ApiError::not_found("Token not found"));
    };
    sqlx::query(r#"UPDATE "mcpAccessToken" SET "revokedAt" = $1 WHERE id = $2"#)
        .bind(now_naive())
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(Json(json!({ "success": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_pat_shape() {
        let (plain, hash, prefix) = generate().unwrap_or_default();
        assert!(plain.starts_with("mcp_pat_"));
        assert_eq!(plain.len(), 8 + 32);
        assert_eq!(prefix.len(), 12);
        assert!(plain.starts_with(&prefix));
        assert_eq!(hash, hash_token(&plain));
        assert_eq!(hash.len(), 64);
    }
}
