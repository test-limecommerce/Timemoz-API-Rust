//! Port `src/audit-logs/audit-prune.service.ts` + `AuditLogsService.pruneExpired` (§8, §0.29).
//!
//! Dua hal, urutannya penting: baris `auditLog` yang lewat 90 hari LEBIH DULU, baru batu nisan
//! pada tiga tabel §2.4 (dibalik, batu nisan hilang lebih dulu dan meninggalkan baris audit
//! `isRestorable` yang menunjuk baris yang sudah lenyap). SQL mentah dengan sengaja: tidak
//! melahirkan baris audit baru dan punya `LIMIT` (batch).

use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, Utc};
use mz_core::AppState;
use mz_jobs::{CronJob, Schedule};
use sqlx::PgPool;

use super::restore::AUDIT_RETENTION_DAYS;

/// `AUDIT_PRUNE_BATCH_SIZE`.
pub const AUDIT_PRUNE_BATCH_SIZE: i64 = 10_000;
const MS_PER_DAY: i64 = 24 * 60 * 60 * 1000;
/// `PRUNE_JOB_TIMEOUT_MS` = 15 menit.
const PRUNE_JOB_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Hasil pemangkasan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PruneResult {
    pub audit_logs: u64,
    pub store_visit_plan: u64,
    pub store_group_user: u64,
    pub store_visit_form_template_user: u64,
}

/// Cron harian 06:00 WIB (`@Cron('0 6 * * *', { timeZone: CRON_TIMEZONE })`); dedup antar
/// replika oleh penjadwal (slot per menit = `jobId` `audit-prune:<tanggal WIB>`).
///
/// Node: src/audit-logs/audit-prune.service.ts:42
pub fn cron_jobs() -> Vec<CronJob> {
    vec![CronJob {
        name: "audit-prune",
        schedule: Schedule::daily(&[6], &[0]),
        timeout: PRUNE_JOB_TIMEOUT,
        run: |state| Box::pin(run_prune(state)),
    }]
}

async fn run_prune(state: AppState) -> anyhow::Result<()> {
    prune_expired(&state.db, None, None, None).await?;
    Ok(())
}

/// Titik batas: `now - retentionDays * MS_PER_DAY`.
pub fn cutoff(now: DateTime<Utc>, retention_days: i64) -> NaiveDateTime {
    (now - chrono::Duration::milliseconds(retention_days * MS_PER_DAY)).naive_utc()
}

/// `pruneInBatches`: `affected === 0 || affected < batchSize` menghentikan perulangan
/// (batch 0 tidak boleh berputar selamanya).
pub(crate) async fn prune_in_batches<F, Fut>(
    batch_size: i64,
    mut delete_batch: F,
) -> sqlx::Result<u64>
where
    F: FnMut(i64) -> Fut,
    Fut: Future<Output = sqlx::Result<u64>>,
{
    let mut removed = 0_u64;
    loop {
        let affected = delete_batch(batch_size).await?;
        removed += affected;
        if affected == 0 || i64::try_from(affected).is_ok_and(|a| a < batch_size) {
            break;
        }
    }
    Ok(removed)
}

/// `pruneExpired({ retentionDays?, batchSize?, now? })`.
pub async fn prune_expired(
    db: &PgPool,
    retention_days: Option<i64>,
    batch_size: Option<i64>,
    now: Option<DateTime<Utc>>,
) -> sqlx::Result<PruneResult> {
    let retention_days = retention_days.unwrap_or(AUDIT_RETENTION_DAYS);
    let batch_size = batch_size.unwrap_or(AUDIT_PRUNE_BATCH_SIZE);
    let cutoff = cutoff(now.unwrap_or_else(Utc::now), retention_days);

    let audit_logs = prune_in_batches(batch_size, |limit| async move {
        sqlx::query(
            r#"DELETE FROM "auditLog"
                WHERE id IN (SELECT id FROM "auditLog" WHERE "createdAt" < $1 LIMIT $2)"#,
        )
        .bind(cutoff)
        .bind(limit)
        .execute(db)
        .await
        .map(|r| r.rows_affected())
    })
    .await?;

    // Klausa `NOT EXISTS` menghormati FK `storeVisit.planId` (Restrict): jadwal yang sudah
    // diklaim kunjungan bukan milik retensi untuk dihapus.
    let plans = prune_in_batches(batch_size, |limit| async move {
        sqlx::query(
            r#"DELETE FROM "storeVisitPlan"
                WHERE id IN (
                  SELECT p.id FROM "storeVisitPlan" p
                   WHERE p."deletedAt" < $1
                     AND NOT EXISTS (SELECT 1 FROM "storeVisit" v WHERE v."planId" = p.id)
                   LIMIT $2)"#,
        )
        .bind(cutoff)
        .bind(limit)
        .execute(db)
        .await
        .map(|r| r.rows_affected())
    })
    .await?;

    let group_members = prune_in_batches(batch_size, |limit| async move {
        sqlx::query(
            r#"DELETE FROM "storeGroupUser"
                WHERE id IN (SELECT id FROM "storeGroupUser" WHERE "deletedAt" < $1 LIMIT $2)"#,
        )
        .bind(cutoff)
        .bind(limit)
        .execute(db)
        .await
        .map(|r| r.rows_affected())
    })
    .await?;

    let form_assignees = prune_in_batches(batch_size, |limit| async move {
        sqlx::query(
            r#"DELETE FROM "storeVisitFormTemplateUser"
                WHERE id IN (SELECT id FROM "storeVisitFormTemplateUser" WHERE "deletedAt" < $1 LIMIT $2)"#,
        )
        .bind(cutoff)
        .bind(limit)
        .execute(db)
        .await
        .map(|r| r.rows_affected())
    })
    .await?;

    tracing::info!(
        "Retensi {retention_days} hari: {audit_logs} baris audit, {plans} jadwal, {group_members} keanggotaan region, {form_assignees} penugasan form dipangkas"
    );

    Ok(PruneResult {
        audit_logs,
        store_visit_plan: plans,
        store_group_user: group_members,
        store_visit_form_template_user: form_assignees,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn schedule_matches_node_cron() {
        let jobs = cron_jobs();
        assert_eq!(jobs.len(), 1);
        let job = &jobs[0];
        assert_eq!(job.name, "audit-prune");
        assert_eq!(job.timeout, Duration::from_secs(900));
        // 06:00 Asia/Jakarta = 23:00 UTC hari sebelumnya.
        let after = DateTime::parse_from_rfc3339("2026-11-16T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let next = job.schedule.next_after(after).unwrap();
        assert_eq!(next.to_rfc3339(), "2026-11-16T23:00:00+00:00");
        let next2 = job.schedule.next_after(next).unwrap();
        assert_eq!(next2.to_rfc3339(), "2026-11-17T23:00:00+00:00");
    }

    #[test]
    fn cutoff_is_90_days_back() {
        let now = DateTime::parse_from_rfc3339("2026-11-16T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(cutoff(now, 90).to_string(), "2026-08-18 00:00:00");
    }

    async fn drive(sizes: Vec<u64>, batch: i64) -> (u64, u64) {
        let calls = AtomicU64::new(0);
        let sizes = std::sync::Mutex::new(sizes.into_iter());
        let total = prune_in_batches(batch, |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            let n = sizes.lock().unwrap().next().unwrap_or(0);
            async move { Ok(n) }
        })
        .await
        .unwrap();
        (total, calls.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn batching_loops_until_short_or_empty_batch() {
        // Dua batch penuh lalu satu pendek → 3 panggilan.
        assert_eq!(drive(vec![10, 10, 4], 10).await, (24, 3));
        // Batch penuh lalu kosong → 2 panggilan.
        assert_eq!(drive(vec![10, 0], 10).await, (10, 2));
        // Kosong sejak awal → 1 panggilan.
        assert_eq!(drive(vec![], 10).await, (0, 1));
        // batchSize 0: `affected === 0` menghentikan (tanpa itu berputar selamanya).
        assert_eq!(drive(vec![], 0).await, (0, 1));
    }
}
