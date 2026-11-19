//! Operasi di dalam transaksi timeline (`tracking-timeline-tx.ts` + method privat
//! `TrackingTimelineService`): lock per tracking, pemindahan isi timeline,
//! pemangkasan manual time APPROVED oleh tracking live, salin baris.

use chrono::NaiveDateTime;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::range::{
    LEGACY_TIMEZONE, ManualTrim, MsRange, OVERLAP_WINDOW_MARGIN_MS, ms_of, ndt, overlap_ms,
    plan_manual_trim, timeline_range_ms,
};

/// `lockTrackingTimelines(tx, trackingId)`.
pub async fn lock_tracking(conn: &mut PgConnection, tracking_id: i32) -> sqlx::Result<()> {
    mz_core::db::advisory_xact_lock(conn, &format!("tracking-timeline:tracking:{tracking_id}"))
        .await
}

/// Tujuan pemindahan isi (`TimelineTarget`).
#[derive(Debug, Clone, Copy)]
pub struct Target {
    pub id: i32,
    pub project_id: i32,
}

/// `moveTimelineChildren(tx, from, target, range?)` — satu round-trip untuk
/// screenshot, app, dan url. `range` = `[gte, lt?)`.
pub async fn move_children(
    conn: &mut PgConnection,
    from: i32,
    target: Target,
    range: Option<(i64, Option<i64>)>,
) -> sqlx::Result<()> {
    let gte = range.map(|(g, _)| ndt(g));
    let lt = range.and_then(|(_, l)| l).map(ndt);
    sqlx::query(
        r#"WITH s AS (
             UPDATE screenshot SET "trackingTimelineId" = $2, "projectId" = $3
             WHERE "trackingTimelineId" = $1
               AND ($4::timestamp IS NULL OR "takenAt" >= $4) AND ($5::timestamp IS NULL OR "takenAt" < $5)
             RETURNING 1
           ), a AS (
             UPDATE app SET "trackingTimelineId" = $2
             WHERE "trackingTimelineId" = $1
               AND ($4::timestamp IS NULL OR "startAt" >= $4) AND ($5::timestamp IS NULL OR "startAt" < $5)
             RETURNING 1
           )
           UPDATE url SET "trackingTimelineId" = $2
           WHERE "trackingTimelineId" = $1
             AND ($4::timestamp IS NULL OR "startAt" >= $4) AND ($5::timestamp IS NULL OR "startAt" < $5)"#,
    )
    .bind(from)
    .bind(target.id)
    .bind(target.project_id)
    .bind(gte)
    .bind(lt)
    .execute(conn)
    .await?;
    Ok(())
}

/// `canMoveChildrenBetweenProjects(from, to)`.
pub async fn can_move_between_projects(
    conn: &mut PgConnection,
    from: i32,
    to: i32,
) -> sqlx::Result<bool> {
    if from == to {
        return Ok(true);
    }
    sqlx::query_scalar(
        r#"SELECT EXISTS(SELECT 1 FROM "projectCompany" WHERE "projectId" = $2)
              AND NOT EXISTS(
                SELECT 1 FROM "projectCompany" t WHERE t."projectId" = $2
                  AND NOT EXISTS(SELECT 1 FROM "projectCompany" f
                                 WHERE f."projectId" = $1 AND f."companyId" = t."companyId"))"#,
    )
    .bind(from)
    .bind(to)
    .fetch_one(conn)
    .await
}

/// Kolom yang disalin `clonedTimelineData` (semua kecuali id/uuid/createdAt/updatedAt;
/// ID entri ClickUp/Jira dikosongkan). Rentang & zona diganti `$2..$3`.
pub async fn clone_timeline(
    conn: &mut PgConnection,
    src: i32,
    range: MsRange,
) -> sqlx::Result<(i32, Uuid)> {
    sqlx::query_as(
        r#"INSERT INTO "trackingTimeline" (note, "isOffline", "startAt", "endAt", "startAtUtc", "endAtUtc",
             "offsetMinutes", timezone, "trackingId", "projectId", status, "manualTimeStatus",
             "manualTimeApprovalSource", reason, "approvedById", "approvedAt", "rejectedReason",
             "requestedAt", billable, "clickupTimeEntryId", "jiraWorklogId", "taskId")
           SELECT note, "isOffline", $2, $3, $2, $3, "offsetMinutes", timezone, "trackingId", "projectId",
             status, "manualTimeStatus", "manualTimeApprovalSource", reason, "approvedById", "approvedAt",
             "rejectedReason", "requestedAt", billable, NULL, NULL, "taskId"
           FROM "trackingTimeline" WHERE id = $1
           RETURNING id, uuid"#,
    )
    .bind(src)
    .bind(ndt(range.start))
    .bind(ndt(range.end))
    .fetch_one(conn)
    .await
}

/// Ubah rentang baris (`timelineRangeData`), opsional sekaligus `timezone`.
pub async fn set_range(
    conn: &mut PgConnection,
    id: i32,
    range: MsRange,
    timezone: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query(
        r#"UPDATE "trackingTimeline" SET "startAt" = $2, "endAt" = $3, "startAtUtc" = $2, "endAtUtc" = $3,
             timezone = COALESCE($4, timezone)
           WHERE id = $1"#,
    )
    .bind(id)
    .bind(ndt(range.start))
    .bind(ndt(range.end))
    .bind(timezone)
    .execute(conn)
    .await?;
    Ok(())
}

/// Kolom baris timeline yang dibutuhkan pemangkasan.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TrimRow {
    pub uuid: Uuid,
    pub tracking_id: i32,
    pub project_id: i32,
    pub start_at: NaiveDateTime,
    pub end_at: NaiveDateTime,
    pub timezone: Option<String>,
    pub row_json: Value,
}

/// Event `manual_time_trimmed_by_live_tracking` (ditulis pemanggil sesudah commit).
#[derive(Debug, Clone)]
pub struct TrimEvent {
    pub tracking_id: i32,
    pub project_id: i32,
    pub removed_seconds: i64,
    pub json: Value,
}

/// `trimApprovedManualTimelines(tx, live, manualIds, liveTimeline)` →
/// (detik terbuang, event).
pub async fn trim_approved_manual(
    conn: &mut PgConnection,
    live: MsRange,
    manual_ids: &[i32],
    live_timeline: Target,
) -> sqlx::Result<(i64, Vec<TrimEvent>)> {
    let mut removed_seconds = 0;
    let mut events = Vec::new();
    for &id in manual_ids {
        let row: Option<TrimRow> = sqlx::query_as(
            r#"SELECT uuid, "trackingId" AS tracking_id, "projectId" AS project_id,
                      "startAt" AS start_at, "endAt" AS end_at, timezone, to_jsonb(tt) AS row_json
               FROM "trackingTimeline" tt WHERE id = $1"#,
        )
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
        let Some(row) = row else { continue };
        let manual = timeline_range_ms(row.start_at, row.end_at, row.timezone.as_deref());
        let Some(plan) = plan_manual_trim(manual, live) else {
            continue;
        };

        let tz = row
            .timezone
            .clone()
            .unwrap_or_else(|| LEGACY_TIMEZONE.to_owned());
        let covered = Some((live.start, Some(live.end)));
        let can_move =
            can_move_between_projects(conn, row.project_id, live_timeline.project_id).await?;
        match plan {
            ManualTrim::Delete => {
                if can_move {
                    move_children(conn, id, live_timeline, None).await?;
                    sqlx::query(
                        r#"UPDATE "manualTimeAttachment" SET "trackingTimelineId" = $2
                           WHERE "trackingTimelineId" = $1"#,
                    )
                    .bind(id)
                    .bind(live_timeline.id)
                    .execute(&mut *conn)
                    .await?;
                    sqlx::query(r#"DELETE FROM "trackingTimeline" WHERE id = $1"#)
                        .bind(id)
                        .execute(&mut *conn)
                        .await?;
                } else {
                    sqlx::query(r#"UPDATE "trackingTimeline" SET status = 0 WHERE id = $1"#)
                        .bind(id)
                        .execute(&mut *conn)
                        .await?;
                }
            }
            ManualTrim::Shrink(range) => {
                if can_move {
                    move_children(conn, id, live_timeline, covered).await?;
                }
                set_range(conn, id, range, Some(&tz)).await?;
            }
            ManualTrim::Split { left, right } => {
                if can_move {
                    move_children(conn, id, live_timeline, covered).await?;
                }
                set_range(conn, id, left, Some(&tz)).await?;
                let (right_id, _) = clone_timeline(conn, id, right).await?;
                move_children(
                    conn,
                    id,
                    Target {
                        id: right_id,
                        project_id: row.project_id,
                    },
                    Some((live.end, None)),
                )
                .await?;
            }
        }

        let removed = overlap_ms(manual, live) / 1000;
        removed_seconds += removed;
        events.push(TrimEvent {
            tracking_id: row.tracking_id,
            project_id: row.project_id,
            removed_seconds: removed,
            json: json!({
                "manualTimelineUuid": row.uuid,
                "trackingId": row.tracking_id,
                "projectId": row.project_id,
                "before": {
                    "startAt": crate::range::iso(ms_of(row.start_at)),
                    "endAt": crate::range::iso(ms_of(row.end_at)),
                },
                "action": plan.to_json(),
                "childrenMovedToLive": can_move,
                "removedSeconds": removed,
                "row": if matches!(plan, ManualTrim::Delete) { row.row_json.clone() } else { Value::Null },
            }),
        });
    }
    Ok((removed_seconds, events))
}

#[derive(sqlx::FromRow)]
struct OtherManual {
    id: i32,
    tracking_id: i32,
    start_at: NaiveDateTime,
    end_at: NaiveDateTime,
    timezone: Option<String>,
}

async fn other_manual_candidates(
    conn: &mut PgConnection,
    live_tracking_id: i32,
    owner: i32,
    live: MsRange,
    only_ids: Option<&[i32]>,
) -> sqlx::Result<Vec<OtherManual>> {
    let rows: Vec<OtherManual> = sqlx::query_as(
        r#"SELECT tt.id, tt."trackingId" AS tracking_id, tt."startAt" AS start_at, tt."endAt" AS end_at, tt.timezone
           FROM "trackingTimeline" tt JOIN tracking t ON t.id = tt."trackingId"
           WHERE tt.status = 1 AND tt."manualTimeStatus" = 'APPROVED' AND tt."trackingId" <> $1
             AND t."userId" = $2 AND tt."startAt" < $3 AND tt."endAt" > $4
             AND ($5::int[] IS NULL OR tt.id = ANY($5))
           ORDER BY tt.id"#,
    )
    .bind(live_tracking_id)
    .bind(owner)
    .bind(ndt(live.end + OVERLAP_WINDOW_MARGIN_MS))
    .bind(ndt(live.start - OVERLAP_WINDOW_MARGIN_MS))
    .bind(only_ids)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter(|r| {
            plan_manual_trim(
                timeline_range_ms(r.start_at, r.end_at, r.timezone.as_deref()),
                live,
            )
            .is_some()
        })
        .collect())
}

/// Hasil `trimOwnerApprovedManualInOtherTrackings`.
pub enum OtherTrim {
    Events(Vec<TrimEvent>),
    /// `from === 'dashboard'` + overlap → `throw new Error('Overlap detected')`.
    OverlapDetected,
}

/// `trimOwnerApprovedManualInOtherTrackings(tx, {...})`.
pub async fn trim_owner_manual_in_other_trackings(
    conn: &mut PgConnection,
    live_tracking_id: i32,
    owner: i32,
    live: MsRange,
    live_timeline: Target,
    dashboard: bool,
) -> sqlx::Result<OtherTrim> {
    let candidates = other_manual_candidates(conn, live_tracking_id, owner, live, None).await?;
    if candidates.is_empty() {
        return Ok(OtherTrim::Events(Vec::new()));
    }
    if dashboard {
        return Ok(OtherTrim::OverlapDetected);
    }
    let mut tracking_ids: Vec<i32> = candidates.iter().map(|c| c.tracking_id).collect();
    tracking_ids.sort_unstable();
    tracking_ids.dedup();
    for id in &tracking_ids {
        lock_tracking(conn, *id).await?;
    }
    let ids: Vec<i32> = candidates.iter().map(|c| c.id).collect();
    let confirmed =
        other_manual_candidates(conn, live_tracking_id, owner, live, Some(&ids)).await?;
    if confirmed.is_empty() {
        return Ok(OtherTrim::Events(Vec::new()));
    }
    let confirmed_ids: Vec<i32> = confirmed.iter().map(|c| c.id).collect();
    let (_, events) = trim_approved_manual(conn, live, &confirmed_ids, live_timeline).await?;

    let mut removed: Vec<(i32, i64)> = Vec::new();
    for e in &events {
        match removed.iter_mut().find(|(t, _)| *t == e.tracking_id) {
            Some((_, s)) => *s += e.removed_seconds,
            None => removed.push((e.tracking_id, e.removed_seconds)),
        }
    }
    removed.sort_by_key(|(t, _)| *t);
    for (tracking_id, seconds) in removed {
        increment_total(conn, tracking_id, -seconds).await?;
    }
    Ok(OtherTrim::Events(events))
}

/// `tracking.update({ totalTakenInSecond: { increment } })` (NULL tetap NULL).
pub async fn increment_total(
    conn: &mut PgConnection,
    tracking_id: i32,
    delta: i64,
) -> sqlx::Result<()> {
    sqlx::query(
        r#"UPDATE tracking SET "totalTakenInSecond" = "totalTakenInSecond" + $2 WHERE id = $1"#,
    )
    .bind(tracking_id)
    .bind(i32::try_from(delta).unwrap_or(if delta < 0 { i32::MIN } else { i32::MAX }))
    .execute(conn)
    .await?;
    Ok(())
}

/// Tulis log event pemangkasan (`appEventLogger().warn(...)`) + hitung ulang
/// budget project manual yang terpotong (`recalculateTrimmedManualBudgets`).
pub fn after_trim(
    state: &mz_core::AppState,
    events: &[TrimEvent],
    live_uuid: &str,
    user_id: i32,
    live_project_id: i32,
) {
    for e in events {
        tracing::warn!(
            target: "app_event",
            event = "manual_time_trimmed_by_live_tracking",
            live_timeline_uuid = live_uuid,
            user_id,
            detail = %e.json,
        );
    }
    let mut projects: Vec<i32> = events
        .iter()
        .map(|e| e.project_id)
        .filter(|p| *p != live_project_id)
        .collect();
    projects.sort_unstable();
    projects.dedup();
    for p in projects {
        crate::spawn::recalculate_burn(state, p);
    }
}
