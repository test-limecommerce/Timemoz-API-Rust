//! `POST /integrations/clickup/webhook` (`clickup-webhook.controller.ts` +
//! `ClickupService.handleWebhook`, `clickup.service.ts:442`). Body mentah
//! (`express.raw({ type: '*/*' })`, `main.ts:52`) diverifikasi HMAC-SHA256 hex dengan
//! `clickupWebhookSecret` mapping; header `x-signature`.

use hmac::{Hmac, Mac};
use mz_core::{ApiError, AppState};
use serde_json::Value;
use sha2::Sha256;

use crate::tasks::{Thrown, upsert_single_task};
use crate::util::{js_str, truthy};

/// `crypto.createHmac('sha256', secret).update(rawBody).digest('hex')`.
pub(crate) fn signature(secret: &str, body: &[u8]) -> String {
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return String::new();
    };
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

/// `handleWebhook(signature, rawBody)`. Galat di luar `try` Node (payload `null`,
/// `webhook_id` non-string untuk Prisma, galat DB saat mencari mapping) → 500.
pub(crate) async fn handle(state: &AppState, sig: &str, raw: &[u8]) -> Result<(), ApiError> {
    let Ok(payload) = serde_json::from_slice::<Value>(raw) else {
        return Ok(());
    };
    if payload.is_null() {
        // `const { webhook_id } = null` → TypeError tak tertangani.
        return Err(ApiError::internal(anyhow::anyhow!(
            "Cannot destructure property 'webhook_id' of 'payload' as it is null."
        )));
    }
    let webhook_id = payload.get("webhook_id");
    let event = payload.get("event");
    let task_id = payload.get("task_id");
    if !truthy(webhook_id) || !truthy(event) {
        return Ok(());
    }
    let Some(webhook_id) = webhook_id.and_then(Value::as_str) else {
        // Prisma menolak nilai non-string untuk kolom String → dilempar.
        return Err(ApiError::internal(anyhow::anyhow!(
            "Invalid value for argument `clickupWebhookId`: expected String"
        )));
    };

    #[allow(clippy::type_complexity)]
    let mapping: Option<(String, Option<String>, i32, i32, Option<String>)> = sqlx::query_as(
        r#"SELECT m."clickupFolderId", m."clickupWebhookSecret", m."projectId", m."companyId",
                  c."clickupApiToken"
           FROM "projectClickupMapping" m JOIN company c ON c.id = m."companyId"
           WHERE m."clickupWebhookId" = $1 AND m.status = 1
           LIMIT 1"#,
    )
    .bind(webhook_id)
    .fetch_optional(&state.db)
    .await
    .map_err(ApiError::internal)?;
    let Some((folder_id, secret, project_id, company_id, token)) = mapping else {
        return Ok(());
    };
    let Some(secret) = secret.filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    if sig != signature(&secret, raw) {
        tracing::warn!("Invalid ClickUp webhook signature for webhook {webhook_id}");
        return Ok(());
    }

    let event_name = js_str(event);
    let r: Result<(), Thrown> = async {
        if event_name == "taskDeleted" && event.is_some_and(Value::is_string) {
            if truthy(task_id) {
                sqlx::query(
                    r#"DELETE FROM task WHERE "externalId" = $1 AND "companyId" = $2
                         AND "projectId" = $3 AND source = 'clickup'"#,
                )
                .bind(js_str(task_id))
                .bind(company_id)
                .bind(project_id)
                .execute(&state.db)
                .await?;
            }
            return Ok(());
        }
        let Some(token) = token.as_deref().filter(|t| !t.is_empty()) else {
            return Ok(());
        };
        if !truthy(task_id) {
            return Ok(());
        }
        upsert_single_task(
            state,
            &js_str(task_id),
            company_id,
            project_id,
            &folder_id,
            token,
        )
        .await
    }
    .await;
    if let Err(Thrown(m)) = r {
        tracing::error!(
            module = "clickup-webhook-handler",
            webhook_id,
            event = %event_name,
            error = %m,
            "clickup webhook"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_node() {
        // crypto.createHmac('sha256','key').update('The quick brown fox jumps over the lazy dog').digest('hex')
        assert_eq!(
            signature("key", b"The quick brown fox jumps over the lazy dog"),
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }
}
