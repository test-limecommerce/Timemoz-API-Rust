//! `manual-time-approvals/approve-manual-time.ts`: `approveManualTimeInTx` +
//! `rejectManualTimeInTx`. Satu-satunya salinan; dipakai auto-approve `createManualTime`
//! (crate ini) dan route `manual-time-approvals` (`mz-timeconfig`).

use chrono::NaiveDateTime;
use sqlx::PgConnection;

use crate::range::{
    MIN_APPROVED_PIECE_MS, OVERLAP_WINDOW_MARGIN_MS, ndt, subtract_ranges, timeline_range_ms,
};
pub use crate::range::{MsRange, format_day_month_year, format_manual_time_seconds};
use crate::tx::{clone_timeline, increment_total, lock_tracking};

/// `ApproveOutcome`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApproveOutcome {
    NotPending,
    Covered {
        requested: MsRange,
    },
    Approved {
        requested: MsRange,
        pieces: Vec<MsRange>,
        adjusted: bool,
        approved_seconds: i64,
        extra_uuids: Vec<String>,
    },
    OverLimit {
        requested: MsRange,
        approved_seconds: i64,
    },
}

impl ApproveOutcome {
    /// `outcome.kind`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NotPending => "not-pending",
            Self::Covered { .. } => "covered",
            Self::Approved { .. } => "approved",
            Self::OverLimit { .. } => "over-limit",
        }
    }
}

#[derive(sqlx::FromRow)]
struct PendingRow {
    manual_time_status: Option<String>,
    start_at: NaiveDateTime,
    end_at: NaiveDateTime,
    timezone: Option<String>,
    tracking_id: i32,
}

/// Potongan yang di-approve: bagian `requested` yang tak tertutup timeline lain.
pub fn approve_pieces(requested: MsRange, others: &[MsRange]) -> Vec<MsRange> {
    subtract_ranges(requested, others, MIN_APPROVED_PIECE_MS)
}

/// `approveManualTimeInTx(tx, timelineId, { approverId, source, maxApprovedSeconds })`.
/// `approver_id` `None` = approve otomatis; `source` = `MANUAL` / `AUTO`.
pub async fn approve_in_tx(
    conn: &mut PgConnection,
    timeline_id: i32,
    approver_id: Option<i32>,
    source: &str,
    max_approved_seconds: Option<f64>,
) -> sqlx::Result<ApproveOutcome> {
    // Pemilik dibaca sebelum lock, seperti `initial` di Node.
    let initial: Option<(i32, Option<i32>)> = sqlx::query_as(
        r#"SELECT t."trackingId", tr."userId" FROM "trackingTimeline" t
           LEFT JOIN tracking tr ON tr.id = t."trackingId" WHERE t.id = $1"#,
    )
    .bind(timeline_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((tracking_id, owner_id)) = initial else {
        return Ok(ApproveOutcome::NotPending);
    };
    lock_tracking(conn, tracking_id).await?;

    let row: Option<PendingRow> = sqlx::query_as(
        r#"SELECT "manualTimeStatus"::text AS manual_time_status, "startAt" AS start_at,
                  "endAt" AS end_at, timezone, "trackingId" AS tracking_id
           FROM "trackingTimeline" WHERE id = $1"#,
    )
    .bind(timeline_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row.filter(|r| r.manual_time_status.as_deref() == Some("PENDING")) else {
        return Ok(ApproveOutcome::NotPending);
    };

    let requested = timeline_range_ms(row.start_at, row.end_at, row.timezone.as_deref());
    let mut pieces = vec![requested];
    if row.timezone.as_deref().is_some_and(|z| !z.is_empty()) {
        // Pemilik tracking null (tak mungkin: onDelete Cascade) → tracking yang sama.
        let others: Vec<(NaiveDateTime, NaiveDateTime, Option<String>)> = sqlx::query_as(
            r#"SELECT t."startAt", t."endAt", t.timezone FROM "trackingTimeline" t
               WHERE t.status = 1 AND t.id <> $1
                 AND (CASE WHEN $2::int IS NULL THEN t."trackingId" = $3
                      ELSE t."trackingId" IN (SELECT id FROM tracking WHERE "userId" = $2) END)
                 AND t."startAt" < $4 AND t."endAt" > $5"#,
        )
        .bind(timeline_id)
        .bind(owner_id)
        .bind(row.tracking_id)
        .bind(ndt(requested.end + OVERLAP_WINDOW_MARGIN_MS))
        .bind(ndt(requested.start - OVERLAP_WINDOW_MARGIN_MS))
        .fetch_all(&mut *conn)
        .await?;
        let ranges: Vec<MsRange> = others
            .iter()
            .map(|(s, e, z)| timeline_range_ms(*s, *e, z.as_deref()))
            .collect();
        pieces = approve_pieces(requested, &ranges);
        if pieces.is_empty() {
            return Ok(ApproveOutcome::Covered { requested });
        }
    }

    // `Math.floor((end - start) / 1000)` per potongan (selalu positif).
    let approved_seconds: i64 = pieces.iter().map(|p| (p.end - p.start) / 1000).sum();
    #[allow(clippy::cast_precision_loss)]
    if max_approved_seconds.is_some_and(|m| approved_seconds as f64 > m) {
        return Ok(ApproveOutcome::OverLimit {
            requested,
            approved_seconds,
        });
    }
    let first = pieces.first().copied().unwrap_or(requested);
    let adjusted = pieces.len() != 1 || first != requested;
    let range = adjusted.then_some(first);
    sqlx::query(
        r#"UPDATE "trackingTimeline" SET status = 1, "manualTimeStatus" = 'APPROVED',
             "approvedById" = $2, "manualTimeApprovalSource" = $3::"ManualTimeApprovalSource",
             "approvedAt" = $4,
             "startAt" = COALESCE($5, "startAt"), "endAt" = COALESCE($6, "endAt"),
             "startAtUtc" = COALESCE($5, "startAtUtc"), "endAtUtc" = COALESCE($6, "endAtUtc")
           WHERE id = $1"#,
    )
    .bind(timeline_id)
    .bind(approver_id)
    .bind(source)
    .bind(mz_core::time::now_naive())
    .bind(range.map(|r| ndt(r.start)))
    .bind(range.map(|r| ndt(r.end)))
    .execute(&mut *conn)
    .await?;

    // Potongan berikutnya = salinan baris yang baru di-approve (status/approver/source/
    // approvedAt ikut; ID ClickUp/Jira tidak; lampiran tetap di baris asli).
    let mut extra_uuids = Vec::new();
    for piece in pieces.iter().skip(1) {
        let (_, uuid) = clone_timeline(conn, timeline_id, *piece).await?;
        extra_uuids.push(uuid.to_string());
    }
    increment_total(conn, row.tracking_id, approved_seconds).await?;
    Ok(ApproveOutcome::Approved {
        requested,
        pieces,
        adjusted,
        approved_seconds,
        extra_uuids,
    })
}

/// `rejectManualTimeInTx`. `false` = sudah tidak PENDING.
pub async fn reject_in_tx(
    conn: &mut PgConnection,
    timeline_id: i32,
    approver_id: i32,
    rejected_reason: &str,
) -> sqlx::Result<bool> {
    let tracking_id: Option<i32> =
        sqlx::query_scalar(r#"SELECT "trackingId" FROM "trackingTimeline" WHERE id = $1"#)
            .bind(timeline_id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some(tracking_id) = tracking_id else {
        return Ok(false);
    };
    lock_tracking(conn, tracking_id).await?;
    let res = sqlx::query(
        r#"UPDATE "trackingTimeline" SET "manualTimeStatus" = 'REJECTED', "rejectedReason" = $3,
           "approvedById" = $2, "approvedAt" = $4
           WHERE id = $1 AND "manualTimeStatus" = 'PENDING'"#,
    )
    .bind(timeline_id)
    .bind(approver_id)
    .bind(rejected_reason)
    .bind(mz_core::time::now_naive())
    .execute(&mut *conn)
    .await?;
    Ok(res.rows_affected() > 0)
}
