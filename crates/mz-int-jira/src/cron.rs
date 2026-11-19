//! Cron `JiraService`: `scheduledSyncAllProjects` (03:20 WIB harian) dan
//! `scheduledRefreshWebhooks` (Senin 01:10 WIB).

use std::time::Duration;

use chrono::{Datelike, NaiveDateTime, Weekday};
use mz_audit::AuditHandle;
use mz_core::AppState;
use mz_jobs::{CRON_TZ, CronJob, Schedule};
use reqwest::Method;
use serde_json::json;

use crate::creds::{Set, needs_refresh, refresh_access_token, register_webhook, update_company};
use crate::http::{self, Error, Res};
use crate::sync::{SyncTarget, sync_issues_internal};

/// Nama job = method Node dalam kebab-case, berawalan `jira-` karena ClickUp punya
/// method `scheduledSyncAllProjects` yang sama (kunci dedup `mz:job:<name>:<slot>` bentrok).
pub const SYNC_JOB: &str = "jira-scheduled-sync-all-projects";
pub const WEBHOOK_JOB: &str = "jira-scheduled-refresh-webhooks";

/// Node: src/integrations/jira/jira.service.ts:1235 (`@Cron('20 3 * * *')` WIB) dan
/// src/integrations/jira/jira.service.ts:1321 (`@Cron('10 1 * * 1')` WIB).
pub fn cron_jobs() -> Vec<CronJob> {
    vec![
        CronJob {
            name: SYNC_JOB,
            schedule: Schedule::daily(&[3], &[20]),
            timeout: Duration::from_secs(60 * 60),
            run: |state| Box::pin(scheduled_sync_all_projects(state)),
        },
        CronJob {
            name: WEBHOOK_JOB,
            schedule: Schedule::weekly(&[Weekday::Mon], &[1], &[10]),
            timeout: Duration::from_secs(15 * 60),
            run: |state| Box::pin(scheduled_refresh_webhooks(state)),
        },
    ]
}

/// `cronLocalWeekday() !== 0` → incremental; Minggu (WIB) → full sync.
pub(crate) fn is_incremental(now: chrono::DateTime<chrono::Utc>) -> bool {
    now.with_timezone(&CRON_TZ).weekday() != Weekday::Sun
}

#[derive(sqlx::FromRow)]
struct MappingRow {
    id: i32,
    #[sqlx(rename = "projectId")]
    project_id: i32,
    #[sqlx(rename = "companyId")]
    company_id: i32,
    #[sqlx(rename = "jiraProjectId")]
    jira_project_id: String,
    #[sqlx(rename = "jiraProjectKey")]
    jira_project_key: String,
    #[sqlx(rename = "jiraAccessToken")]
    access_token: Option<String>,
    #[sqlx(rename = "jiraRefreshToken")]
    refresh_token: Option<String>,
    #[sqlx(rename = "jiraCloudId")]
    cloud_id: Option<String>,
    #[sqlx(rename = "jiraSiteUrl")]
    site_url: Option<String>,
    #[sqlx(rename = "jiraTokenExpiresAt")]
    expires_at: Option<NaiveDateTime>,
}

/// `scheduledSyncAllProjects()`.
pub async fn scheduled_sync_all_projects(state: AppState) -> anyhow::Result<()> {
    tracing::info!("Starting scheduled Jira sync...");
    let mappings: Vec<MappingRow> = sqlx::query_as(
        r#"SELECT m.id, m."projectId", m."companyId", m."jiraProjectId", m."jiraProjectKey",
                  c."jiraAccessToken", c."jiraRefreshToken", c."jiraCloudId", c."jiraSiteUrl",
                  c."jiraTokenExpiresAt"
           FROM "projectJiraMapping" m JOIN company c ON c.id = m."companyId"
           WHERE m.status = 1 ORDER BY m.id"#,
    )
    .fetch_all(&state.db)
    .await?;
    let incremental = is_incremental(chrono::Utc::now());
    // Tulisan `company` dari cron tanpa konteks request (Node: tanpa `runAs`).
    let audit = AuditHandle::detached(state.db.clone());

    // Kelompokkan per company (urutan kemunculan pertama, seperti `Map`).
    let mut groups: Vec<(i32, Vec<&MappingRow>)> = Vec::new();
    for m in &mappings {
        match groups.iter_mut().find(|(c, _)| *c == m.company_id) {
            Some((_, list)) => list.push(m),
            None => groups.push((m.company_id, vec![m])),
        }
    }

    let mut synced = 0_usize;
    for (company_id, list) in &groups {
        let Some(first) = list.first() else { continue };
        let (Some(cloud_id), Some(access)) = (
            first.cloud_id.as_deref().filter(|s| !s.is_empty()),
            first.access_token.as_deref().filter(|s| !s.is_empty()),
        ) else {
            continue;
        };
        let mut token = access.to_owned();
        if needs_refresh(first.expires_at) {
            let Some(refresh) = first.refresh_token.as_deref().filter(|s| !s.is_empty()) else {
                continue;
            };
            match refresh_access_token(&state, &audit, *company_id, refresh).await {
                Ok(t) => token = t,
                Err(err) => {
                    let ids: Vec<i32> = list.iter().map(|m| m.id).collect();
                    tracing::error!(
                        module = "jira-scheduled-sync",
                        company_id,
                        mapping_ids = ?ids,
                        error = %err,
                        "Jira token refresh failed"
                    );
                    continue;
                }
            }
        }
        for m in list {
            let target = SyncTarget {
                company_id: *company_id,
                project_id: m.project_id,
                jira_project_id: &m.jira_project_id,
                jira_project_key: &m.jira_project_key,
                site_url: first.site_url.as_deref(),
                access_token: &token,
                cloud_id,
            };
            match sync_issues_internal(&state, &target, true, incremental).await {
                Ok(_) => synced += 1,
                Err(err) => tracing::error!(
                    module = "jira-scheduled-sync",
                    mapping_id = m.id,
                    error = %err,
                    "Jira scheduled sync failed"
                ),
            }
        }
    }
    tracing::info!(
        "Scheduled Jira sync ({}) complete: {synced}/{} projects synced",
        if incremental { "incremental" } else { "full" },
        mappings.len()
    );
    Ok(())
}

type WebhookCompany = (
    i32,
    String,
    String,
    Option<String>,
    Option<NaiveDateTime>,
    i32,
);

async fn refresh_one(state: &AppState, audit: &AuditHandle, c: &WebhookCompany) -> Res<bool> {
    let (id, cloud_id, access, refresh, expires, webhook_id) = c;
    let mut token = access.clone();
    if needs_refresh(*expires) {
        let Some(refresh) = refresh.as_deref().filter(|s| !s.is_empty()) else {
            return Ok(false);
        };
        token = refresh_access_token(state, audit, *id, refresh).await?;
    }
    let res = http::send(
        http::bearer(
            state,
            Method::PUT,
            &http::jira_url(state, cloud_id, "/webhook/refresh"),
            &token,
        )
        .json(&json!({ "webhookIds": [webhook_id] })),
    )
    .await;
    match res {
        Ok(_) => {}
        Err(err) if err.status() == Some(410) => {
            update_company(
                state,
                audit,
                *id,
                &[
                    Set::Int("jiraWebhookId", None),
                    Set::Text("jiraWebhookSecret", None),
                ],
            )
            .await?;
            register_webhook(state, audit, *id, &token, cloud_id).await?;
            tracing::info!("Jira webhook re-registered for company {id} after expiry");
        }
        Err(err) => return Err(Error::Http(err)),
    }
    Ok(true)
}

/// `scheduledRefreshWebhooks()`.
pub async fn scheduled_refresh_webhooks(state: AppState) -> anyhow::Result<()> {
    tracing::info!("Starting scheduled Jira webhook refresh...");
    let companies: Vec<WebhookCompany> = sqlx::query_as(
        r#"SELECT id, "jiraCloudId", "jiraAccessToken", "jiraRefreshToken", "jiraTokenExpiresAt",
                  "jiraWebhookId"
           FROM company
           WHERE "jiraWebhookId" IS NOT NULL AND "jiraCloudId" IS NOT NULL
             AND "jiraAccessToken" IS NOT NULL
           ORDER BY id"#,
    )
    .fetch_all(&state.db)
    .await?;
    let audit = AuditHandle::detached(state.db.clone());
    let mut refreshed = 0_usize;
    for c in &companies {
        match refresh_one(&state, &audit, c).await {
            Ok(true) => refreshed += 1,
            Ok(false) => {}
            Err(err) => tracing::error!(
                module = "jira-webhook-refresh",
                company_id = c.0,
                error = %err,
                "Jira webhook refresh failed"
            ),
        }
    }
    tracing::info!(
        "Jira webhook refresh complete: {refreshed}/{} refreshed",
        companies.len()
    );
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn weekday_in_wib() {
        // Sabtu 2026-11-14 18:00 UTC = Minggu 01:00 WIB → full.
        let sat_utc = chrono::Utc
            .with_ymd_and_hms(2026, 11, 14, 18, 0, 0)
            .unwrap();
        assert!(!is_incremental(sat_utc));
        let sun_utc = chrono::Utc
            .with_ymd_and_hms(2026, 11, 15, 18, 0, 0)
            .unwrap();
        assert!(is_incremental(sun_utc));
    }

    #[test]
    fn schedules() {
        let jobs = cron_jobs();
        assert_eq!(jobs.len(), 2);
        // Senin 2026-11-16 01:10 WIB = Minggu 18:10 UTC.
        let after = chrono::Utc.with_ymd_and_hms(2026, 11, 15, 0, 0, 0).unwrap();
        let next = jobs[1].schedule.next_after(after).unwrap();
        assert_eq!(
            next,
            chrono::Utc
                .with_ymd_and_hms(2026, 11, 15, 18, 10, 0)
                .unwrap()
        );
        let next = jobs[0].schedule.next_after(after).unwrap();
        assert_eq!(
            next,
            chrono::Utc
                .with_ymd_and_hms(2026, 11, 15, 20, 20, 0)
                .unwrap()
        );
    }
}
