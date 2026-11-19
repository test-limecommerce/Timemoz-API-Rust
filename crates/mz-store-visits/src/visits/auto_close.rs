//! Port `src/store-visits/store-visit-auto-close.service.ts` + `store-visits.cron.ts` (§5.4):
//! tiap jam (menit :10) batalkan kunjungan aktif yang melewati `storeVisitMaxDurationHours`
//! company-nya, lalu sapu foto belum terkonfirmasi. `storeVisit` bukan model ber-audit.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::NaiveDateTime;
use mz_core::AppState;
use mz_core::time::now_naive;
use mz_jobs::{CronJob, Schedule};
use sqlx::PgPool;

use crate::photos::{UNCONFIRMED_MAX_AGE_HOURS, cleanup_unconfirmed_photos};

/// `DEFAULT_MAX_DURATION_HOURS`.
pub const DEFAULT_MAX_DURATION_HOURS: i32 = 8;

const MS_PER_HOUR: i64 = 60 * 60 * 1000;

/// `AutoCloseResult`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutoCloseResult {
    pub scanned: usize,
    pub expired: usize,
    pub cancelled: u64,
    pub companies_processed: usize,
    pub companies_failed: usize,
}

/// Kandidat: `(visitId, companyId, checkInAt, company.storeVisitMaxDurationHours)`.
pub type StaleCandidate = (i32, i32, NaiveDateTime, Option<i32>);

/// Bagian murni: kunjungan yang MELEWATI (`>`) batas per company, dikelompokkan per company
/// (urut id company).
pub fn expired_by_company(
    candidates: &[StaleCandidate],
    now: NaiveDateTime,
) -> BTreeMap<i32, Vec<i32>> {
    let now_ms = now.and_utc().timestamp_millis();
    let mut out: BTreeMap<i32, Vec<i32>> = BTreeMap::new();
    for (id, company_id, check_in_at, hours) in candidates {
        let hours = hours.unwrap_or(DEFAULT_MAX_DURATION_HOURS);
        if now_ms - check_in_at.and_utc().timestamp_millis() <= i64::from(hours) * MS_PER_HOUR {
            continue;
        }
        out.entry(*company_id).or_default().push(*id);
    }
    out
}

/// `autoCloseStaleVisits(now)`: satu query untuk semua company, tulis per company (galat satu
/// company tidak menghentikan yang lain), tulisan dijaga `state = active`.
pub async fn auto_close_stale_visits(
    db: &PgPool,
    now: NaiveDateTime,
) -> sqlx::Result<AutoCloseResult> {
    let candidates: Vec<StaleCandidate> = sqlx::query_as(
        r#"SELECT v.id, v."companyId", v."checkInAt", c."storeVisitMaxDurationHours"
             FROM "storeVisit" v LEFT JOIN company c ON c.id = v."companyId"
            WHERE v.state = 'active'::"StoreVisitState""#,
    )
    .fetch_all(db)
    .await?;
    let groups = expired_by_company(&candidates, now);
    let mut result = AutoCloseResult {
        scanned: candidates.len(),
        expired: groups.values().map(Vec::len).sum(),
        ..Default::default()
    };
    for (company_id, ids) in &groups {
        let res = sqlx::query(
            r#"UPDATE "storeVisit" SET state = 'cancelled'::"StoreVisitState"
                WHERE id = ANY($1) AND state = 'active'::"StoreVisitState""#,
        )
        .bind(ids)
        .execute(db)
        .await;
        match res {
            Ok(r) => {
                result.cancelled += r.rows_affected();
                result.companies_processed += 1;
            }
            Err(e) => {
                result.companies_failed += 1;
                tracing::error!(
                    "auto-close failed for company {company_id} ({} visit(s)): {e}",
                    ids.len()
                );
            }
        }
    }
    if result.expired > 0 || result.companies_failed > 0 {
        let failed = if result.companies_failed > 0 {
            format!(", {} company/companies failed", result.companies_failed)
        } else {
            String::new()
        };
        tracing::info!(
            "auto-close: scanned {} active visit(s), cancelled {} of {} expired across {} company/companies{failed}",
            result.scanned,
            result.cancelled,
            result.expired,
            result.companies_processed
        );
    }
    Ok(result)
}

/// `SweepResult`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepResult {
    pub auto_close: Option<AutoCloseResult>,
    /// `(deletedRows, deletedObjects)`.
    pub photos: Option<(u64, u64)>,
}

/// `runHourlySweep()`: dua pekerjaan, masing-masing dengan penangkap galatnya sendiri.
pub async fn run_hourly_sweep(state: &AppState) -> SweepResult {
    let mut out = SweepResult::default();
    match auto_close_stale_visits(&state.db, now_naive()).await {
        Ok(r) => out.auto_close = Some(r),
        Err(e) => tracing::error!("auto-close sweep failed: {e}"),
    }
    match cleanup_unconfirmed_photos(state, UNCONFIRMED_MAX_AGE_HOURS).await {
        Ok(r) => out.photos = Some(r),
        Err(e) => tracing::error!("unconfirmed-photo cleanup failed: {e:?}"),
    }
    out
}

// ───────────────────────── cron ─────────────────────────

/// `StoreVisitsCron.running` (hanya dalam satu proses).
static RUNNING: AtomicBool = AtomicBool::new(false);

fn try_begin() -> bool {
    RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

/// Penanda dilepas juga saat future dibatalkan (timeout) — padanan `finally`.
struct Release;

impl Drop for Release {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

/// Batas satu jalan (Node tanpa batas).
const SWEEP_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// `@Cron('10 * * * *') handleHourlySweep`.
pub fn cron_jobs() -> Vec<CronJob> {
    vec![CronJob {
        name: "store-visits-hourly-sweep",
        schedule: Schedule::hourly(&[10]),
        timeout: SWEEP_TIMEOUT,
        run: |state| Box::pin(handle_hourly_sweep(state)),
    }]
}

/// Node: src/store-visits/store-visits.cron.ts:30 `handleHourlySweep`. Tidak pernah gagal.
pub async fn handle_hourly_sweep(state: AppState) -> anyhow::Result<()> {
    if !try_begin() {
        tracing::warn!("previous store-visit sweep still running, skipping tick");
        return Ok(());
    }
    let _release = Release;
    run_hourly_sweep(&state).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as CDuration;

    fn now() -> NaiveDateTime {
        NaiveDateTime::parse_from_str("2026-08-20 12:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
    }

    fn visit(id: i32, company: i32, hours_old: i64, limit: Option<i32>) -> StaleCandidate {
        (id, company, now() - CDuration::hours(hours_old), limit)
    }

    /// store-visit-auto-close.service.spec.ts:98–158.
    #[test]
    fn thresholds_per_company() {
        assert_eq!(
            expired_by_company(&[visit(1, 10, 9, Some(8))], now()).get(&10),
            Some(&vec![1])
        );
        assert!(expired_by_company(&[visit(1, 10, 7, Some(8))], now()).is_empty());
        // exactly 8h — not exceeded yet
        assert!(expired_by_company(&[visit(1, 10, 8, Some(8))], now()).is_empty());
        let g = expired_by_company(
            &[
                visit(1, 10, 10, Some(8)),
                visit(2, 10, 5, Some(8)),
                visit(3, 20, 10, Some(12)),
                visit(4, 20, 13, Some(12)),
            ],
            now(),
        );
        assert_eq!(g.get(&10), Some(&vec![1]));
        assert_eq!(g.get(&20), Some(&vec![4]));
        let g = expired_by_company(&[visit(1, 10, 9, None), visit(2, 10, 7, None)], now());
        assert_eq!(g.get(&10), Some(&vec![1]));
    }

    /// store-visits.cron.spec.ts — "skips a tick that arrives while the previous run is still going".
    #[test]
    fn overlap_guard() {
        assert!(try_begin());
        assert!(!try_begin());
        drop(Release);
        assert!(try_begin());
        drop(Release);
        assert_eq!(cron_jobs().len(), 1);
    }
}
