//! Cron analytics: evaluasi milestone harian dan sweeper retry. Port
//! `AnalyticsMilestonesService` (`src/analytics/analytics-milestones.service.ts`):
//! `evaluateMilestones` (`@Cron('30 5 * * *', WIB)`) dan `sweepRetries`
//! (`@Cron('7,37 * * * *')`). Konsumer `milestone.run` / `retry.sweep` (concurrency 1)
//! dijalankan langsung di dalam run cron.

use std::collections::{HashMap, HashSet};

use chrono::{Duration, NaiveDateTime, Utc};
use mz_audit::AuditHandle;
use mz_core::AppState;
use mz_core::time::now_naive;
use serde_json::{Value, json};

use crate::analytics::{
    self, APP_INSTALLED, DAY_7_STILL_TRACKING, DAY_30_RETAINED, FIRST_TRACKED_HOUR,
};
use crate::ga::{MAX_SEND_ATTEMPTS, send_all};

/// `MILESTONE_BATCH_SIZE`.
pub const MILESTONE_BATCH_SIZE: i64 = 500;
/// `FIRST_TRACKED_HOUR_SECONDS`.
pub const FIRST_TRACKED_HOUR_SECONDS: i64 = 3600;
/// `DAY_7_TRACKING_DAYS` / `DAY_30_TRACKING_DAYS`.
pub const DAY_7_TRACKING_DAYS: i64 = 7;
pub const DAY_30_TRACKING_DAYS: i64 = 30;
/// `RETRY_MIN_AGE_MS` (10 menit).
pub const RETRY_MIN_AGE_MINUTES: i64 = 10;
/// `STUCK_SENDING_AGE_MS` (15 menit).
pub const STUCK_SENDING_AGE_MINUTES: i64 = 15;
/// `RETRY_SWEEP_LIMIT`.
pub const RETRY_SWEEP_LIMIT: i64 = 200;

/// `users.status` yang dievaluasi: `STATUS_ACTIVE` dan `STATUS_DISABLED`.
const EVALUATED_USER_STATUSES: [i32; 2] = [1, 3];
/// `ACTIVE_MEMBERSHIP` (`userCompany.status`).
const ACTIVE_MEMBERSHIP: i32 = 1;
/// `MILESTONE_EVENTS`: ketiganya harus ada agar user dilewati.
const MILESTONE_EVENTS: [&str; 3] = [FIRST_TRACKED_HOUR, DAY_7_STILL_TRACKING, DAY_30_RETAINED];

/// Pemberi info sebuah event milik user (`analyticsEvents` yang di-select).
type UserEvents = HashMap<i32, Vec<(String, Option<Value>)>>;

/// Cron `analytics-milestones` (`Schedule::daily(&[5], &[30])`, 05:30 WIB).
/// Node: `analytics-milestones.service.ts:73` `scheduleMilestoneRun` -> `evaluateMilestones` (:131).
pub async fn run_milestones(state: AppState) -> anyhow::Result<()> {
    // Node menulis audit `users.updateMany` tanpa konteks request (system).
    let audit = AuditHandle::detached(state.db.clone());
    let started = Utc::now();
    let mut cursor: i32 = 0;
    let mut scanned = 0usize;
    let mut claimed = 0usize;

    loop {
        let batch: Vec<(i32, NaiveDateTime)> = sqlx::query_as(
            r#"SELECT id, "createdAt" FROM users
               WHERE id > $1 AND "createdAt" >= $2 AND status = ANY($3)
               ORDER BY id ASC LIMIT $4"#,
        )
        .bind(cursor)
        .bind(analytics::new_user_cutoff(&state))
        .bind(EVALUATED_USER_STATUSES.to_vec())
        .bind(MILESTONE_BATCH_SIZE)
        .fetch_all(&state.db)
        .await?;
        let Some((last_id, _)) = batch.last().copied() else {
            break;
        };
        cursor = last_id;
        scanned += batch.len();

        let ids: Vec<i32> = batch.iter().map(|(id, _)| *id).collect();
        let events = load_events(&state, &ids).await?;
        let companies = load_default_companies(&state, &ids).await?;

        let pending: Vec<(i32, NaiveDateTime)> = batch
            .into_iter()
            .filter(|(id, _)| !has_all_milestones(events.get(id)))
            .collect();
        if pending.is_empty() {
            continue;
        }
        let pending_ids: Vec<i32> = pending.iter().map(|(id, _)| *id).collect();
        let stats = aggregate_tracking(&state, &pending_ids).await?;

        let mut to_send = Vec::new();
        for (user_id, created_at) in &pending {
            let Some(stat) = stats.get(user_id) else {
                continue;
            };
            let user_events = events.get(user_id).map(Vec::as_slice).unwrap_or_default();
            let ctx = MilestoneCtx {
                user_id: *user_id,
                created_at: *created_at,
                company_id: companies.get(user_id).copied(),
                platform: platform_from_app_installed(user_events),
            };
            to_send.extend(claim_milestones_for(&state, &audit, &ctx, user_events, *stat).await);
        }
        claimed += to_send.len();
        send_all(&state, to_send).await;
    }

    tracing::info!(
        scanned,
        claimed,
        ms = (Utc::now() - started).num_milliseconds(),
        "Evaluasi milestone selesai"
    );
    Ok(())
}

/// `user.userCompany[0]` dengan `nestedUserCompanyScope()`: membership aktif, default dulu, id terkecil.
async fn load_default_companies(
    state: &AppState,
    user_ids: &[i32],
) -> sqlx::Result<HashMap<i32, i32>> {
    let rows: Vec<(i32, i32)> = sqlx::query_as(
        r#"SELECT DISTINCT ON (uc."userId") uc."userId", uc."companyId"
           FROM "userCompany" uc
           WHERE uc."userId" = ANY($1) AND uc.status = $2
           ORDER BY uc."userId", uc."isDefault" DESC, uc.id ASC"#,
    )
    .bind(user_ids)
    .bind(ACTIVE_MEMBERSHIP)
    .fetch_all(&state.db)
    .await?;
    Ok(rows.into_iter().collect())
}

/// `analyticsEvents` milik kandidat yang namanya milestone atau `app_installed`.
async fn load_events(state: &AppState, user_ids: &[i32]) -> sqlx::Result<UserEvents> {
    let names: Vec<&str> = MILESTONE_EVENTS
        .iter()
        .copied()
        .chain(std::iter::once(APP_INSTALLED))
        .collect();
    let rows: Vec<(i32, String, Option<Value>)> = sqlx::query_as(
        r#"SELECT "userId", "eventName", params FROM "analyticsEvent"
           WHERE "userId" = ANY($1) AND "eventName" = ANY($2)"#,
    )
    .bind(user_ids)
    .bind(&names)
    .fetch_all(&state.db)
    .await?;
    let mut map: UserEvents = HashMap::new();
    for (user_id, name, params) in rows {
        map.entry(user_id).or_default().push((name, params));
    }
    Ok(map)
}

fn has_all_milestones(events: Option<&Vec<(String, Option<Value>)>>) -> bool {
    let owned: HashSet<&str> = events
        .map(|list| list.iter().map(|(n, _)| n.as_str()).collect())
        .unwrap_or_default();
    MILESTONE_EVENTS.iter().all(|name| owned.contains(name))
}

#[derive(Debug, Clone, Copy)]
pub struct TrackingStats {
    pub total_seconds: i64,
    pub tracking_days: i64,
}

/// `aggregateTracking`: durasi dari `trackingTimeline`, bukan `tracking.totalTakenInSecond`.
pub async fn aggregate_tracking(
    state: &AppState,
    user_ids: &[i32],
) -> sqlx::Result<HashMap<i32, TrackingStats>> {
    if user_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(i32, i64, i64)> = sqlx::query_as(
        r#"SELECT t."userId" AS user_id,
                  COALESCE(SUM(EXTRACT(EPOCH FROM (tl."endAt" - tl."startAt"))), 0)::bigint AS total_seconds,
                  COUNT(DISTINCT t."takenDate") AS tracking_days
           FROM "tracking" t
           JOIN "trackingTimeline" tl ON tl."trackingId" = t.id
           WHERE t."userId" = ANY($1::int[])
             AND tl.status = 1
             AND tl."endAt" > tl."startAt"
           GROUP BY t."userId""#,
    )
    .bind(user_ids)
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(user_id, total_seconds, tracking_days)| {
            (
                user_id,
                TrackingStats {
                    total_seconds,
                    tracking_days,
                },
            )
        })
        .collect())
}

struct MilestoneCtx {
    user_id: i32,
    created_at: NaiveDateTime,
    company_id: Option<i32>,
    platform: Option<String>,
}

/// `platformFromAppInstalled`: dibaca dari param `app_installed`, bukan tebakan dari `os`.
fn platform_from_app_installed(events: &[(String, Option<Value>)]) -> Option<String> {
    events
        .iter()
        .find(|(name, _)| name == APP_INSTALLED)
        .and_then(|(_, params)| params.as_ref())
        .and_then(|p| p.get("platform"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// `claimMilestonesFor`: klaim milestone yang ambangnya terlewati. Mengembalikan id baris
/// yang berhasil diklaim. Galat per klaim dicatat dan dilewati (`claimAndEmit` Node tidak melempar).
async fn claim_milestones_for(
    state: &AppState,
    audit: &AuditHandle,
    ctx: &MilestoneCtx,
    events: &[(String, Option<Value>)],
    stat: TrackingStats,
) -> Vec<i32> {
    let owned: HashSet<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    let now = Utc::now();
    let days_since_signup = ((now.timestamp_millis()
        - ctx.created_at.and_utc().timestamp_millis())
    .div_euclid(86_400_000))
    .max(0);
    let company = ctx.company_id.map_or(Value::Null, Value::from);
    let platform = ctx.platform.clone().map_or(Value::Null, Value::String);

    let mut plan: Vec<(&str, Vec<(&str, Value)>)> = Vec::new();
    if !owned.contains(FIRST_TRACKED_HOUR) && stat.total_seconds >= FIRST_TRACKED_HOUR_SECONDS {
        plan.push((
            FIRST_TRACKED_HOUR,
            vec![
                ("platform", platform),
                ("company_id", company.clone()),
                ("total_seconds", json!(stat.total_seconds)),
            ],
        ));
    }
    // Definisinya akumulatif (hari distinct), `days_since_signup` ikut dikirim untuk analisis.
    for (name, threshold) in [
        (DAY_7_STILL_TRACKING, DAY_7_TRACKING_DAYS),
        (DAY_30_RETAINED, DAY_30_TRACKING_DAYS),
    ] {
        if owned.contains(name) || stat.tracking_days < threshold {
            continue;
        }
        plan.push((
            name,
            vec![
                ("company_id", company.clone()),
                ("distinct_tracking_days", json!(stat.tracking_days)),
                ("days_since_signup", json!(days_since_signup)),
            ],
        ));
    }

    let mut claimed = Vec::new();
    for (name, params) in plan {
        match analytics::claim_event(state, audit, ctx.user_id, name, params, now_naive()).await {
            Ok(Some(id)) => claimed.push(id),
            Ok(None) => {}
            Err(err) => tracing::error!(
                user_id = ctx.user_id,
                event = name,
                error = %err,
                "Gagal klaim event milestone"
            ),
        }
    }
    claimed
}

/// Cron `analytics-retry-sweep` (`Schedule::hourly(&[7, 37])`).
/// Node: `analytics-milestones.service.ts:83` `scheduleRetrySweep` -> `sweepRetries` (:307).
pub async fn run_retry_sweep(state: AppState) -> anyhow::Result<()> {
    let now = now_naive();
    let stale_before = now - Duration::minutes(RETRY_MIN_AGE_MINUTES);

    match retry_candidates(&state, stale_before).await {
        Ok(ids) => {
            if !ids.is_empty() {
                tracing::info!(count = ids.len(), "Sweeper mengantre ulang event GA");
            }
            if i64::try_from(ids.len()).unwrap_or(i64::MAX) == RETRY_SWEEP_LIMIT {
                tracing::warn!(
                    limit = RETRY_SWEEP_LIMIT,
                    "Sweeper mencapai batas baris — sisanya menunggu sapuan berikutnya"
                );
            }
            send_all(&state, ids).await;
        }
        Err(err) => tracing::error!(error = %err, "Sweeper retry gagal"),
    }

    report_stuck_sending(&state, now - Duration::minutes(STUCK_SENDING_AGE_MINUTES)).await;
    Ok(())
}

/// Baris `PENDING` yang layak di-retry. `SENDING` tidak pernah di-retry otomatis.
pub async fn retry_candidates(
    state: &AppState,
    stale_before: NaiveDateTime,
) -> sqlx::Result<Vec<i32>> {
    sqlx::query_scalar(
        r#"SELECT id FROM "analyticsEvent"
           WHERE status = 'PENDING' AND attempts < $1
             AND ((("sendingAt" IS NULL) AND "createdAt" < $2) OR "sendingAt" < $2)
           ORDER BY id ASC
           LIMIT $3"#,
    )
    .bind(MAX_SEND_ATTEMPTS)
    .bind(stale_before)
    .bind(RETRY_SWEEP_LIMIT)
    .fetch_all(&state.db)
    .await
}

/// Baris SENDING lebih tua dari ambang = proses mati di tengah kirim. Hanya dilaporkan.
async fn report_stuck_sending(state: &AppState, older_than: NaiveDateTime) {
    let res: sqlx::Result<i64> = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "analyticsEvent"
           WHERE status = 'SENDING' AND "sendingAt" < $1"#,
    )
    .bind(older_than)
    .fetch_one(&state.db)
    .await;
    match res {
        Ok(0) => {}
        Ok(stuck) => tracing::warn!(
            stuck,
            "analyticsEvent nyangkut di SENDING > {STUCK_SENDING_AGE_MINUTES} menit — perlu ditinjau manual"
        ),
        Err(err) => tracing::error!(error = %err, "Gagal memeriksa baris SENDING"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn milestone_detection() {
        let all = vec![
            (FIRST_TRACKED_HOUR.to_owned(), None),
            (DAY_7_STILL_TRACKING.to_owned(), None),
            (DAY_30_RETAINED.to_owned(), None),
        ];
        assert!(has_all_milestones(Some(&all)));
        assert!(!has_all_milestones(
            all.get(..2).map(|s| s.to_vec()).as_ref()
        ));
        assert!(!has_all_milestones(None));
    }

    #[test]
    fn platform_is_read_from_app_installed_params() {
        let events = vec![
            (
                FIRST_TRACKED_HOUR.to_owned(),
                Some(json!({ "platform": "web" })),
            ),
            (APP_INSTALLED.to_owned(), Some(json!({ "platform": "ios" }))),
        ];
        assert_eq!(platform_from_app_installed(&events).as_deref(), Some("ios"));
        assert_eq!(platform_from_app_installed(&events[..1]), None);
    }
}
