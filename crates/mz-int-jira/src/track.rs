//! `JiraService.trackWorklog` (`jira.service.ts:1389`): worklog di issue Jira sesudah timeline
//! dibuat/diubah. Dipanggil dari `mz-timelines` (kerja latar).

use chrono::{DateTime, Utc};
use mz_audit::{ActorType, AuditHandle};
use mz_core::AppState;
use reqwest::Method;
use serde_json::{Value, json};

use crate::creds::valid_access_token;
use crate::http::{self, Res};

/// `started`: `toISOString()` dengan `Z` → `+0000` (format yang diminta Jira).
pub fn worklog_started(start_ms: i64) -> String {
    let at = DateTime::<Utc>::from_timestamp_millis(start_ms).unwrap_or_default();
    mz_core::time::to_js_iso(&at).replace('Z', "+0000")
}

/// `trackWorklog(userId, issueKey, startAt, endAt, description?, existingWorklogId?)` → id
/// worklog. Tidak pernah gagal: galat dicatat lalu `None` (Node `catch → null`).
///
/// Refresh token yang terjadi di sini dicatat ke audit sebagai aktor `system`
/// (`jira-token-refresh`): di Node update itu jalan fire-and-forget sesudah respons, sehingga
/// entri audit-nya biasanya hilang.
pub async fn track_worklog(
    state: &AppState,
    user_id: i32,
    issue_key: &str,
    start_ms: i64,
    end_ms: i64,
    description: Option<&str>,
    existing: Option<&str>,
) -> Option<String> {
    let audit = AuditHandle::run_as(
        state.db.clone(),
        ActorType::System,
        Some("jira-token-refresh"),
    );
    match track(
        state,
        &audit,
        user_id,
        issue_key,
        start_ms,
        end_ms,
        description,
        existing,
    )
    .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("Jira track worklog failed: {e}");
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn track(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    issue_key: &str,
    start_ms: i64,
    end_ms: i64,
    description: Option<&str>,
    existing: Option<&str>,
) -> Res<Option<String>> {
    let Some(token) = valid_access_token(state, audit, user_id).await? else {
        return Ok(None);
    };
    let (email, account): (Option<String>, Option<String>) = sqlx::query_as(
        r#"SELECT (SELECT username FROM users WHERE id = $1 LIMIT 1),
                  (SELECT "jiraAccountId" FROM "userJiraMapping"
                    WHERE "userId" = $1 AND "companyId" = $2 AND status = 1 LIMIT 1)"#,
    )
    .bind(user_id)
    .bind(token.company_id)
    .fetch_one(&state.db)
    .await?;
    let comment = [
        description.unwrap_or_default().to_owned(),
        email
            .filter(|e| !e.is_empty())
            .map(|e| format!("Tracked by: {e}"))
            .unwrap_or_default(),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join(" | ");
    #[allow(clippy::cast_possible_truncation)]
    let spent = ((end_ms - start_ms) as f64 / 1000.0).round() as i64;
    if spent < 60 {
        return Ok(None);
    }
    let mut body = json!({
        "timeSpentSeconds": spent,
        "started": worklog_started(start_ms),
        "comment": {
            "type": "doc", "version": 1,
            "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": comment }] }],
        },
    });
    if let (Some(acc), Some(obj)) = (account.filter(|a| !a.is_empty()), body.as_object_mut()) {
        obj.insert("author".into(), json!({ "accountId": acc }));
    }
    let url = http::jira_url(
        state,
        &token.cloud_id,
        &format!("/issue/{issue_key}/worklog"),
    );
    if let Some(id) = existing {
        http::send(
            http::bearer(
                state,
                Method::PUT,
                &format!("{url}/{id}"),
                &token.access_token,
            )
            .json(&body),
        )
        .await?;
        return Ok(Some(id.to_owned()));
    }
    let data = http::send(http::bearer(state, Method::POST, &url, &token.access_token).json(&body))
        .await?;
    Ok(data.get("id").and_then(Value::as_str).map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn started_format() {
        // 2026-05-07T10:00:00.000Z
        assert_eq!(
            worklog_started(1_778_148_000_000),
            "2026-05-07T10:00:00.000+0000"
        );
    }
}
