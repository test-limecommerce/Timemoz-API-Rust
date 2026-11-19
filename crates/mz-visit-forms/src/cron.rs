//! Port `store-visit-forms.cron.ts`: penyapu file & aset kop/kaki yang tidak pernah
//! dikonfirmasi (§6.5 sumber ke-1). Mati bawaan sampai diaktifkan `MZ_JOBS`.

use std::time::Duration;

use mz_core::AppState;
use mz_jobs::{CronJob, Schedule};

use crate::files::{
    UNCONFIRMED_MAX_AGE_HOURS, cleanup_unconfirmed_files, cleanup_unconfirmed_template_assets,
    end_sweep, try_begin_sweep,
};

/// Batas satu jalan (Node tanpa batas; penyapuan per baris, jadi longgar).
const SWEEP_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// `@Cron('15 * * * *') handleHourlySweep`.
pub fn cron_jobs() -> Vec<CronJob> {
    vec![CronJob {
        name: "handle-hourly-sweep",
        schedule: Schedule::hourly(&[15]),
        timeout: SWEEP_TIMEOUT,
        run: |state| Box::pin(handle_hourly_sweep(state)),
    }]
}

/// Penanda dilepas juga saat future dibatalkan (timeout) — padanan `finally`.
struct Release;

impl Drop for Release {
    fn drop(&mut self) {
        end_sweep();
    }
}

/// Node: src/store-visit-forms/store-visit-forms.cron.ts:30 `handleHourlySweep`.
/// Dua penyapuan, masing-masing dengan penangkap galatnya sendiri; tidak pernah gagal.
pub async fn handle_hourly_sweep(state: AppState) -> anyhow::Result<()> {
    if !try_begin_sweep() {
        tracing::warn!("previous store-visit-form file sweep still running, skipping tick");
        return Ok(());
    }
    let _release = Release;
    sweep(
        "file",
        cleanup_unconfirmed_files(&state, UNCONFIRMED_MAX_AGE_HOURS).await,
    );
    sweep(
        "template asset",
        cleanup_unconfirmed_template_assets(&state, UNCONFIRMED_MAX_AGE_HOURS).await,
    );
    Ok(())
}

fn sweep<T>(label: &str, result: anyhow::Result<T>) {
    if let Err(e) = result {
        tracing::error!("store-visit-form {label} sweep failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_one_hourly_job() {
        let jobs = cron_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "handle-hourly-sweep");
        assert_eq!(jobs[0].schedule.minutes, &[15]);
        assert!(jobs[0].schedule.hours.is_none());
    }

    #[test]
    fn guard_skips_overlapping_run_and_releases() {
        assert!(try_begin_sweep());
        assert!(!try_begin_sweep());
        {
            let _r = Release;
        }
        assert!(try_begin_sweep());
        end_sweep();
    }
}
