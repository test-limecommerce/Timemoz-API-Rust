//! `@Cron('30 2 * * *', { timeZone: CRON_TIMEZONE }) scheduledSyncAllProjects`
//! (`clickup.service.ts:506`): 02:30 WIB. Malam biasa menarik task yang berubah 25 jam
//! terakhir (`date_updated_gt`); hari Minggu (WIB) sinkron penuh.

use std::time::Duration;

use chrono::{DateTime, Datelike, Utc, Weekday};
use mz_core::AppState;
use mz_jobs::{CRON_TZ, CronJob, Schedule};

use crate::tasks::{Thrown, sync_tasks_internal};

/// `INCREMENTAL_WINDOW_MS` = 25 jam.
const INCREMENTAL_WINDOW_MS: i64 = 25 * 60 * 60 * 1000;
/// Node tanpa batas waktu; batas penjadwal Rust dibuat longgar.
const JOB_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);

/// Cron ClickUp untuk didaftarkan orkestrator.
///
/// Node: src/integrations/clickup/clickup.service.ts:506
pub fn cron_jobs() -> Vec<CronJob> {
    vec![CronJob {
        name: "clickup-scheduled-sync-all-projects",
        schedule: Schedule::daily(&[2], &[30]),
        timeout: JOB_TIMEOUT,
        run: |state| Box::pin(scheduled_sync_all_projects(state)),
    }]
}

/// `updatedAfterMs`: `None` (penuh) pada hari Minggu WIB.
pub(crate) fn updated_after_ms(now: DateTime<Utc>) -> Option<i64> {
    let full = now.with_timezone(&CRON_TZ).weekday() == Weekday::Sun;
    (!full).then(|| now.timestamp_millis() - INCREMENTAL_WINDOW_MS)
}

/// `scheduledSyncAllProjects()`.
pub async fn scheduled_sync_all_projects(state: AppState) -> anyhow::Result<()> {
    let now = Utc::now();
    let after = updated_after_ms(now);
    tracing::info!(
        "Starting scheduled ClickUp sync ({})...",
        if after.is_none() {
            "full"
        } else {
            "incremental"
        }
    );
    let mappings: Vec<(i32, i32, i32, String, Option<String>)> = sqlx::query_as(
        r#"SELECT m.id, m."projectId", m."companyId", m."clickupFolderId", c."clickupApiToken"
           FROM "projectClickupMapping" m JOIN company c ON c.id = m."companyId"
           WHERE m.status = 1
           ORDER BY m.id"#,
    )
    .fetch_all(&state.db)
    .await?;

    let mut synced = 0_usize;
    for (mapping_id, project_id, company_id, folder_id, token) in &mappings {
        let Some(token) = token.as_deref().filter(|t| !t.is_empty()) else {
            continue;
        };
        match sync_tasks_internal(
            &state,
            *company_id,
            *project_id,
            folder_id,
            token,
            true,
            false,
            after,
            true,
        )
        .await
        {
            Ok(_) => synced += 1,
            Err(Thrown(m)) => tracing::error!(
                module = "clickup-scheduled-sync",
                mapping_id,
                error = %m,
                "clickup scheduled sync"
            ),
        }
    }
    tracing::info!(
        "Scheduled ClickUp sync complete: {synced}/{} projects synced",
        mappings.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sunday_wib_is_full() {
        // Sabtu 2026-11-14 19:30 UTC = Minggu 02:30 WIB.
        let sun = DateTime::parse_from_rfc3339("2026-11-14T19:30:00Z")
            .map(|d| d.with_timezone(&Utc))
            .ok();
        assert_eq!(sun.map(updated_after_ms), Some(None));
        // Minggu 2026-11-15 19:30 UTC = Senin 02:30 WIB.
        let mon = DateTime::parse_from_rfc3339("2026-11-15T19:30:00Z")
            .map(|d| d.with_timezone(&Utc))
            .ok();
        assert_eq!(
            mon.map(updated_after_ms),
            mon.map(|m| Some(m.timestamp_millis() - INCREMENTAL_WINDOW_MS))
        );
        let jobs = cron_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            jobs.first().map(|j| j.name),
            Some("clickup-scheduled-sync-all-projects")
        );
    }
}
