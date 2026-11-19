//! Penjadwal cron pengganti `@nestjs/schedule` + Bull `jobId` dedup.
//!
//! - Jadwal ditulis sebagai [`Schedule`] (subset cron yang dipakai Node: menit, jam,
//!   hari-minggu, zona waktu). Tidak ada dependensi crate cron eksternal.
//! - Setiap replika menghitung jadwal yang sama; hanya replika yang memenangkan
//!   `SET NX` Redis `mz:job:<name>:<slot>` yang menjalankan slot itu (setara jobId
//!   deterministik Bull di Node, mis. `overtime-tick:<YYYYMMDDHH>`). Advisory lock
//!   Postgres sengaja TIDAK dipakai: produksi lewat pgbouncer (lihat
//!   `Mudah-API/src/audit-logs/audit-prune.service.ts`).
//! - Job MATI secara bawaan. Diaktifkan lewat env `MZ_JOBS` (`all` atau daftar nama
//!   dipisah koma). Tujuannya: selama Node masih menjalankan cron yang sama, Rust tidak
//!   ikut menjalankannya (dua penjadwal = dua kali kirim email / dua kali hapus).
//!
//! Registry job ada di `mz-server` (`src/jobs.rs`), supaya crate ini tidak bergantung
//! pada crate domain. Crate domain BOLEH memakai [`claim_once`] untuk dedup sendiri.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use chrono::{DateTime, Datelike, Duration as ChronoDuration, Timelike, Utc, Weekday};
use chrono_tz::Tz;
use fred::prelude::*;
use fred::types::{Expiration, SetOptions};
use mz_core::AppState;

/// `Asia/Jakarta` — `CRON_TIMEZONE` Node (`src/date/cron-timezone.ts`).
pub const CRON_TZ: Tz = chrono_tz::Asia::Jakarta;

pub type JobFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;
pub type JobFn = fn(AppState) -> JobFuture;

/// Subset ekspresi cron yang dipakai Node. `None` = `*`.
#[derive(Clone, Copy, Debug)]
pub struct Schedule {
    pub minutes: &'static [u32],
    pub hours: Option<&'static [u32]>,
    pub weekdays: Option<&'static [Weekday]>,
    pub tz: Tz,
}

impl Schedule {
    /// `@Cron('M * * * *')` (zona tidak relevan untuk jadwal per jam).
    pub const fn hourly(minutes: &'static [u32]) -> Self {
        Self {
            minutes,
            hours: None,
            weekdays: None,
            tz: chrono_tz::UTC,
        }
    }

    /// `@Cron('M H * * *', { timeZone: CRON_TIMEZONE })`.
    pub const fn daily(hours: &'static [u32], minutes: &'static [u32]) -> Self {
        Self {
            minutes,
            hours: Some(hours),
            weekdays: None,
            tz: CRON_TZ,
        }
    }

    /// `@Cron('M H * * D', { timeZone: CRON_TIMEZONE })`.
    pub const fn weekly(
        weekdays: &'static [Weekday],
        hours: &'static [u32],
        minutes: &'static [u32],
    ) -> Self {
        Self {
            minutes,
            hours: Some(hours),
            weekdays: Some(weekdays),
            tz: CRON_TZ,
        }
    }

    pub const fn with_tz(mut self, tz: Tz) -> Self {
        self.tz = tz;
        self
    }

    fn matches(&self, at: DateTime<Utc>) -> bool {
        let local = at.with_timezone(&self.tz);
        self.minutes.contains(&local.minute())
            && self.hours.is_none_or(|h| h.contains(&local.hour()))
            && self.weekdays.is_none_or(|w| w.contains(&local.weekday()))
    }

    /// Menit penuh berikutnya (> `after`) yang cocok. Dicari menit demi menit,
    /// maksimal 8 hari (jadwal mingguan); `None` bila jadwal tidak pernah cocok.
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let base = after.with_second(0)?.with_nanosecond(0)?;
        (1..=8 * 24 * 60)
            .map(|m| base + ChronoDuration::minutes(m))
            .find(|t| self.matches(*t))
    }
}

/// Satu cron. `run` menerima `AppState` (clone) dan harus menelan galat per-item
/// sendiri seperti Node (`try/catch` + log); galat yang dikembalikan hanya dilog.
#[derive(Clone, Copy)]
pub struct CronJob {
    /// Nama stabil, dipakai di env `MZ_JOBS` dan kunci dedup.
    pub name: &'static str,
    pub schedule: Schedule,
    /// Batas waktu satu run (Bull `timeout`); lewat batas → future dibatalkan + log.
    pub timeout: Duration,
    pub run: JobFn,
}

/// Nama job yang diaktifkan env `MZ_JOBS`. Kosong/tidak diset = tidak ada.
pub fn enabled_names(state: &AppState) -> Vec<String> {
    state
        .config
        .var("MZ_JOBS")
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn is_enabled(enabled: &[String], name: &str) -> bool {
    enabled.iter().any(|n| n == "all" || n == name)
}

/// `SET key 1 NX EX ttl`. `Ok(true)` = pemanggil pemilik slot ini.
/// Galat Redis dikembalikan; pemanggil memutuskan (penjadwal: lewati slot).
pub async fn claim_once(state: &AppState, key: &str, ttl: Duration) -> anyhow::Result<bool> {
    let secs = i64::try_from(ttl.as_secs().max(1)).unwrap_or(i64::MAX);
    let res: Option<String> = state
        .redis
        .next()
        .set(
            key,
            "1",
            Some(Expiration::EX(secs)),
            Some(SetOptions::NX),
            false,
        )
        .await?;
    Ok(res.is_some())
}

/// Jalankan satu job sekarang (tanpa dedup) — untuk `mudah-rs job <nama>` dan test.
pub async fn run_now(state: &AppState, job: &CronJob) -> anyhow::Result<()> {
    match tokio::time::timeout(job.timeout, (job.run)(state.clone())).await {
        Ok(r) => r,
        Err(_) => anyhow::bail!("job {} timeout setelah {:?}", job.name, job.timeout),
    }
}

/// Mulai loop penjadwal untuk setiap job yang aktif. Mengembalikan handle task.
/// `gates` = nama `MZ_JOBS` yang bukan cron (gerbang jalur request, mis. `overtime-recalc`),
/// hanya supaya tidak dilaporkan sebagai nama tak dikenal.
pub fn spawn(
    state: &AppState,
    jobs: &[CronJob],
    gates: &[&str],
) -> Vec<tokio::task::JoinHandle<()>> {
    let enabled = enabled_names(state);
    for name in &enabled {
        if name != "all" && !gates.contains(&name.as_str()) && !jobs.iter().any(|j| j.name == name)
        {
            tracing::warn!(job = %name, "MZ_JOBS menyebut job yang tidak dikenal");
        }
    }
    jobs.iter()
        .filter(|j| is_enabled(&enabled, j.name))
        .map(|job| {
            let job = *job;
            let state = state.clone();
            tracing::info!(job = job.name, "cron aktif");
            tokio::spawn(async move { job_loop(state, job).await })
        })
        .collect()
}

async fn job_loop(state: AppState, job: CronJob) {
    loop {
        let now = Utc::now();
        let Some(next) = job.schedule.next_after(now) else {
            tracing::error!(job = job.name, "jadwal tidak pernah cocok; loop berhenti");
            return;
        };
        let wait = (next - now).to_std().unwrap_or(Duration::ZERO);
        tokio::time::sleep(wait).await;

        let slot = next.format("%Y%m%d%H%M");
        let key = format!("mz:job:{}:{slot}", job.name);
        // TTL = 1 hari: jauh di atas selisih jam antar replika, di bawah periode harian berikutnya
        // tidak perlu karena slot unik per menit.
        match claim_once(&state, &key, Duration::from_secs(86_400)).await {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                tracing::error!(job = job.name, error = %e, "gagal klaim slot cron; slot dilewati");
                continue;
            }
        }
        let started = std::time::Instant::now();
        tracing::info!(job = job.name, %slot, "cron mulai");
        match run_now(&state, &job).await {
            Ok(()) => tracing::info!(
                job = job.name,
                ms = started.elapsed().as_millis() as u64,
                "cron selesai"
            ),
            Err(e) => tracing::error!(job = job.name, error = %e, "cron gagal"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .unwrap_or_default()
    }

    #[test]
    fn hourly_next() {
        let s = Schedule::hourly(&[10]);
        assert_eq!(
            s.next_after(utc(2026, 11, 16, 8, 9)),
            Some(utc(2026, 11, 16, 8, 10))
        );
        assert_eq!(
            s.next_after(utc(2026, 11, 16, 8, 10)),
            Some(utc(2026, 11, 16, 9, 10))
        );
    }

    #[test]
    fn half_hourly() {
        let s = Schedule::hourly(&[0, 30]);
        assert_eq!(
            s.next_after(utc(2026, 11, 16, 8, 1)),
            Some(utc(2026, 11, 16, 8, 30))
        );
    }

    #[test]
    fn daily_wib() {
        // 00:05 WIB = 17:05 UTC hari sebelumnya.
        let s = Schedule::daily(&[0], &[5]);
        assert_eq!(
            s.next_after(utc(2026, 11, 16, 12, 0)),
            Some(utc(2026, 11, 16, 17, 5))
        );
    }

    #[test]
    fn weekly_monday_wib() {
        // Senin 01:10 WIB = Minggu 18:10 UTC. 2026-11-16 = Senin.
        let s = Schedule::weekly(&[Weekday::Mon], &[1], &[10]);
        assert_eq!(
            s.next_after(utc(2026, 11, 16, 0, 0)),
            Some(utc(2026, 11, 22, 18, 10))
        );
    }

    #[test]
    fn enabled_parsing() {
        let e = vec!["overtime-tick".to_owned()];
        assert!(is_enabled(&e, "overtime-tick"));
        assert!(!is_enabled(&e, "audit-prune"));
        assert!(is_enabled(&["all".to_owned()], "audit-prune"));
    }
}
