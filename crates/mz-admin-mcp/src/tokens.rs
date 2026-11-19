//! Port `src/admin-mcp/tokens/*`: `POST|GET /admin/mcp-tokens`, `DELETE /admin/mcp-tokens/:uuid`
//! (JwtAdminAuthGuard; tanpa `@OperatorRoles`).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use mz_core::time::{JsDate, now_naive};
use mz_core::uuid_rules::parse_uuid_pipe;
use mz_core::{ApiError, ApiResult, AppState};
use mz_operator::Operator;
use rand::TryRngCore;
use serde_json::{Value, json};

use crate::body::{Body, V, str_of};
use crate::pat::{PAT_PREFIX, hash_token};

/// `AdminPatService.generate()`: `mcp_admin_pat_` + 16 byte acak (hex), prefix 16 karakter.
fn generate() -> ApiResult<(String, String, String)> {
    let mut buf = [0_u8; 16];
    rand::rngs::OsRng
        .try_fill_bytes(&mut buf)
        .map_err(ApiError::internal)?;
    let plain = format!("{PAT_PREFIX}{}", hex::encode(buf));
    let hash = hash_token(&plain);
    let prefix: String = plain.chars().take(16).collect();
    Ok((plain, hash, prefix))
}

/// Node: src/admin-mcp/tokens/tokens.controller.ts:28 (`POST`, 201)
pub async fn h_create(
    State(state): State<AppState>,
    op: Operator,
    Body(b): Body,
) -> ApiResult<(StatusCode, Json<Value>)> {
    // `CreateAdminTokenDto`: @IsString @IsNotEmpty @MaxLength(255) (pipe global).
    let mut v = V::default();
    v.max_length("name", b.get("name"), 255);
    v.not_empty_string("name", b.get("name"));
    v.finish()?;
    let name = str_of(&b, "name").unwrap_or_default();

    let (plain, hash, prefix) = generate()?;
    let (uuid, name, token_prefix): (String, String, String) = sqlx::query_as(
        r#"INSERT INTO "mcpAdminAccessToken" ("operatorId", name, "tokenHash", "tokenPrefix", "expiresAt", status)
           VALUES ($1, $2, $3, $4, NULL, 1)
           RETURNING uuid::text, name, "tokenPrefix""#,
    )
    .bind(op.id)
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

/// Node: src/admin-mcp/tokens/tokens.controller.ts:34 (`GET`)
pub async fn h_list(State(state): State<AppState>, op: Operator) -> ApiResult<Json<Value>> {
    let rows: Vec<ListRow> = sqlx::query_as(
        r#"SELECT uuid::text, name, "tokenPrefix", scopes, "lastUsedAt", "expiresAt", "revokedAt", "createdAt"
             FROM "mcpAdminAccessToken"
            WHERE "operatorId" = $1 AND status = 1
            ORDER BY "createdAt" DESC"#,
    )
    .bind(op.id)
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

/// Node: src/admin-mcp/tokens/tokens.controller.ts:40 (`DELETE :uuid`, `ParseUUIDPipe`)
pub async fn h_revoke(
    State(state): State<AppState>,
    op: Operator,
    Path(raw): Path<String>,
) -> ApiResult<Json<Value>> {
    let uuid = parse_uuid_pipe(&raw)?;
    let id: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "mcpAdminAccessToken"
            WHERE uuid = $1 AND "operatorId" = $2 AND status = 1 AND "revokedAt" IS NULL"#,
    )
    .bind(uuid)
    .bind(op.id)
    .fetch_optional(&state.db)
    .await?;
    let Some(id) = id else {
        return Err(ApiError::not_found("Token not found"));
    };
    sqlx::query(r#"UPDATE "mcpAdminAccessToken" SET "revokedAt" = $1 WHERE id = $2"#)
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
        assert!(plain.starts_with("mcp_admin_pat_"));
        assert_eq!(plain.len(), 14 + 32);
        assert_eq!(prefix.len(), 16);
        assert!(plain.starts_with(&prefix));
        assert_eq!(hash, hash_token(&plain));
        assert_eq!(hash.len(), 64);
    }
}
