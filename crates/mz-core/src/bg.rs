//! Tugas latar (fire-and-forget) yang DILACAK, supaya bisa ditunggu saat shutdown.
//!
//! Masalah: `tokio::spawn` biasa membuat tugas seperti kirim email atau flush audit
//! lepas begitu saja. Saat deploy (SIGTERM dari Docker), proses berhenti dan tugas
//! yang sedang jalan ikut terputus. Modul ini memakai [`TaskTracker`] global:
//! [`spawn`] mendaftarkan tugas, lalu [`drain`] menunggunya sampai batas waktu
//! (`SHUTDOWN_DRAIN_SECS`, bawaan 8 detik, di bawah grace Docker 10 detik).

use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

use tokio_util::task::TaskTracker;

/// Bawaan `SHUTDOWN_DRAIN_SECS` bila env tidak diset atau tidak valid.
const DEFAULT_DRAIN_SECS: u64 = 8;

static TRACKER: OnceLock<TaskTracker> = OnceLock::new();

fn tracker() -> &'static TaskTracker {
    TRACKER.get_or_init(TaskTracker::new)
}

/// Jalankan `fut` di latar belakang dan catat di pelacak global.
/// Pengganti `tokio::spawn` untuk pekerjaan sekali jalan (bukan loop jangka panjang).
pub fn spawn<F>(fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    tracker().spawn(fut);
}

/// Batas waktu tunggu tugas latar saat shutdown, dari `SHUTDOWN_DRAIN_SECS`.
pub fn drain_timeout(config: &crate::config::Config) -> Duration {
    let secs = config
        .var("SHUTDOWN_DRAIN_SECS")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_DRAIN_SECS);
    Duration::from_secs(secs)
}

/// Tutup pelacak lalu tunggu semua tugas latar selesai, paling lama `timeout`.
///
/// Mengembalikan jumlah tugas yang MASIH jalan (0 = semua selesai). Tugas yang
/// masih jalan setelah batas waktu tidak dibatalkan di sini; ia ikut berhenti
/// bersama runtime saat proses keluar.
pub async fn drain(timeout: Duration) -> usize {
    drain_tracker(tracker(), timeout).await
}

/// Logika [`drain`] untuk pelacak mana pun (dipisah agar bisa diuji tanpa state global).
async fn drain_tracker(tracker: &TaskTracker, timeout: Duration) -> usize {
    tracker.close();
    let pending = tracker.len();
    if pending == 0 {
        tracing::info!("shutdown: tidak ada tugas latar yang berjalan");
        return 0;
    }
    tracing::info!(pending, "shutdown: menunggu tugas latar selesai");
    match tokio::time::timeout(timeout, tracker.wait()).await {
        Ok(()) => {
            tracing::info!("shutdown: semua tugas latar selesai");
            0
        }
        Err(_) => {
            let left = tracker.len();
            tracing::warn!(
                left,
                timeout_secs = timeout.as_secs(),
                "shutdown: batas waktu tercapai, tugas latar masih berjalan dan akan terputus"
            );
            left
        }
    }
}

/// Selesai saat SIGINT (Ctrl+C) atau SIGTERM (sinyal stop Docker) diterima.
/// Dipakai sebagai `with_graceful_shutdown` oleh orkestrator.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "gagal memasang handler SIGTERM, hanya Ctrl+C yang dipakai");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Memakai pelacak lokal (bukan global) supaya test paralel tidak saling mengganggu.

    #[tokio::test]
    async fn drain_returns_zero_when_short_tasks_finish() {
        let t = TaskTracker::new();
        for _ in 0..5 {
            t.spawn(async {
                tokio::time::sleep(Duration::from_millis(5)).await;
            });
        }
        assert_eq!(drain_tracker(&t, Duration::from_secs(2)).await, 0);
    }

    #[tokio::test]
    async fn drain_reports_pending_when_task_outlives_timeout() {
        let t = TaskTracker::new();
        t.spawn(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        assert!(drain_tracker(&t, Duration::from_millis(50)).await > 0);
    }

    #[test]
    fn drain_timeout_defaults_to_eight_seconds() {
        let cfg = crate::config::Config::for_test();
        assert_eq!(drain_timeout(&cfg), Duration::from_secs(8));
    }

    #[test]
    fn drain_timeout_reads_env_override() {
        let mut cfg = crate::config::Config::for_test();
        cfg.var_overrides
            .insert("SHUTDOWN_DRAIN_SECS".into(), " 3 ".into());
        assert_eq!(drain_timeout(&cfg), Duration::from_secs(3));
    }
}
