//! `ClickupService.trackTaskTimeComment` (`clickup.service.ts:1417`): komentar durasi di task
//! ClickUp sesudah timeline dibuat/diubah. Dipanggil dari `mz-timelines` (kerja latar).

use mz_core::AppState;
use serde_json::{Value, json};

use crate::client::Client;
use crate::service::credentials;
use crate::util::js_str;

/// `formatTrackedDuration(totalSeconds)`: `1h 2m 5s`, `0s`.
pub fn format_tracked_duration(total_seconds: i64) -> String {
    let h = total_seconds / 3600;
    let m = (total_seconds % 3600) / 60;
    let s = total_seconds % 60;
    let mut parts = Vec::new();
    if h > 0 {
        parts.push(format!("{h}h"));
    }
    if m > 0 {
        parts.push(format!("{m}m"));
    }
    if s > 0 || parts.is_empty() {
        parts.push(format!("{s}s"));
    }
    parts.join(" ")
}

/// Teks komentar: `⏱️ <aktor> tracked <durasi> on this task via Timemoz` (+ `Note: ...`).
pub fn comment_text(actor: &str, start_ms: i64, end_ms: i64, description: Option<&str>) -> String {
    #[allow(clippy::cast_possible_truncation)]
    let secs = ((end_ms - start_ms) as f64 / 1000.0).round().max(0.0) as i64;
    let note = description.unwrap_or_default().trim();
    let mut lines = vec![format!(
        "\u{23f1}\u{fe0f} {actor} tracked {} on this task via Timemoz",
        format_tracked_duration(secs)
    )];
    if !note.is_empty() {
        lines.push(format!("Note: {note}"));
    }
    lines.join("\n")
}

/// `trackTaskTimeComment(userId, externalId, startAt, endAt, description?, existingCommentId?)`
/// → id komentar. Tidak pernah gagal: semua galat dicatat lalu `None` (Node `catch → null`).
pub async fn track_task_time_comment(
    state: &AppState,
    user_id: i32,
    external_id: &str,
    start_ms: i64,
    end_ms: i64,
    description: Option<&str>,
    existing: Option<&str>,
) -> Option<String> {
    match track(
        state,
        user_id,
        external_id,
        start_ms,
        end_ms,
        description,
        existing,
    )
    .await
    {
        Ok(id) => id,
        Err(msg) => {
            tracing::error!("ClickUp track time comment failed: {msg}");
            None
        }
    }
}

async fn track(
    state: &AppState,
    user_id: i32,
    external_id: &str,
    start_ms: i64,
    end_ms: i64,
    description: Option<&str>,
    existing: Option<&str>,
) -> Result<Option<String>, String> {
    let creds = credentials(state, user_id)
        .await
        .map_err(|e| e.to_string())?;
    let Some(token) = creds.as_ref().and_then(|c| c.token()) else {
        tracing::warn!("[CLICKUP-DEBUG] trackTaskTimeComment: ABORT — missing clickupApiToken");
        return Ok(None);
    };
    let user: Option<(Option<String>, Option<String>)> =
        sqlx::query_as(r#"SELECT name, username FROM users WHERE id = $1 LIMIT 1"#)
            .bind(user_id)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| e.to_string())?;
    let actor = match user {
        Some((name, username)) => {
            let suffix = username
                .filter(|u| !u.is_empty())
                .map(|u| format!(" ({u})"))
                .unwrap_or_default();
            format!("{}{suffix}", name.unwrap_or_default())
                .trim()
                .to_owned()
        }
        None => format!("user #{user_id}"),
    };
    let text = comment_text(&actor, start_ms, end_ms, description);
    let client = Client::new(state, token);
    let fail = |e: crate::client::HttpErr| {
        tracing::error!(
            "[CLICKUP-DEBUG] trackTaskTimeComment: FAILED status={}",
            e.status
                .map_or_else(|| "undefined".to_owned(), |s| s.to_string())
        );
        e.message
    };
    if let Some(id) = existing {
        client
            .put(&format!("/comment/{id}"), &json!({ "comment_text": text }))
            .await
            .map_err(fail)?;
        return Ok(Some(id.to_owned()));
    }
    let data = client
        .post(
            &format!("/task/{external_id}/comment"),
            &json!({ "comment_text": text, "notify_all": false }),
        )
        .await
        .map_err(fail)?;
    // `resp.data?.id != null ? String(resp.data.id) : null`.
    Ok(match data.get("id") {
        None | Some(Value::Null) => None,
        id => Some(js_str(id)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_and_text() {
        assert_eq!(format_tracked_duration(0), "0s");
        assert_eq!(format_tracked_duration(3725), "1h 2m 5s");
        assert_eq!(format_tracked_duration(3600), "1h");
        assert_eq!(
            comment_text("Ani (ani@x.test)", 0, 90_400, Some("  rapat  ")),
            "\u{23f1}\u{fe0f} Ani (ani@x.test) tracked 1m 30s on this task via Timemoz\nNote: rapat"
        );
        assert_eq!(
            comment_text("Budi (b@x.id)", 0, 3_725_400, Some("  fitur login ")),
            "\u{23f1}\u{fe0f} Budi (b@x.id) tracked 1h 2m 5s on this task via Timemoz\nNote: fitur login"
        );
        assert_eq!(
            comment_text("user #3", 0, 499, None),
            "\u{23f1}\u{fe0f} user #3 tracked 0s on this task via Timemoz"
        );
        // Durasi negatif dijepit ke 0.
        assert_eq!(
            comment_text("Ani", 5_000, 0, None),
            "\u{23f1}\u{fe0f} Ani tracked 0s on this task via Timemoz"
        );
    }
}
