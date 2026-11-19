//! Antrean job tahan restart di atas Redis (generik, dipakai Mixpanel dan nanti yang lain).
//!
//! Node memakai Bull; format Bull khusus Node, jadi di sini skemanya sendiri dengan
//! kunci yang TERPISAH dari Bull (tidak ada bentrok dengan proses Node yang masih jalan):
//!
//! - `mz:q:<name>:due`  — ZSET, member = id job, score = epoch ms kapan job boleh diambil.
//! - `mz:q:<name>:jobs` — HASH, id -> JSON [`StoredJob`]
//!   (`{data, attempts, maxAttempts, backoffMs, enqueuedAt}`).
//!
//! Semantik:
//! - [`enqueue`] atomik (Lua `HSETNX` + `ZADD NX`): id yang sama tidak membuat job ganda.
//! - Klaim atomik: ambil paling banyak N job yang `score <= now`, lalu geser score-nya ke
//!   `now + lease` (visibility timeout). Worker yang mati di tengah job tidak mengunci job:
//!   setelah lease habis job otomatis diambil lagi -> **at-least-once**. Handler wajib
//!   idempoten (Mixpanel: dedup `$insert_id`).
//! - Sukses/final = `ZREM` + `HDEL`. Retry = `attempts` naik, score = `now + backoffMs * 2^(attempts-1)`
//!   (eksponensial ala Bull). Panic atau timeout handler diperlakukan sebagai retry.
//! - Hitungan `attempts` hanya naik saat handler benar-benar selesai (retry), bukan saat klaim,
//!   jadi lease yang kedaluwarsa karena proses mati tidak menghabiskan jatah percobaan.
//!
//! Tambahan opsional (kompatibel; antrean yang tidak memakainya berperilaku persis seperti dulu):
//! - `mz:q:<name>:paused` — kunci penanda jeda ([`pause`]/[`resume`]). Selama ada, [`claim`]
//!   tidak menyerahkan job (job yang sedang berjalan tidak terpengaruh; enqueue tetap diterima).
//!   Dicek di dalam skrip klaim, jadi berlaku lintas worker/instance/restart.
//! - `mz:q:<name>:limit` — limiter jarak mulai ([`spawn_worker_limited`]): paling banyak SATU
//!   job diklaim per `spacing` untuk seluruh antrean (global di Redis, setara `limiter` Bull
//!   `{max: 1, duration}`).
//! - [`enqueue_many`] (satu round trip per 500 job) dan [`clear_waiting`] (hapus job yang
//!   menunggu, bukan yang sedang di-lease).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use fred::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use crate::AppState;

/// Lease bawaan worker (visibility timeout). Harus > [`DEFAULT_HANDLER_TIMEOUT`].
pub const DEFAULT_LEASE: Duration = Duration::from_secs(60);
/// Batas waktu satu eksekusi handler (di bawah lease, supaya job tidak dobel diambil saat berjalan).
pub const DEFAULT_HANDLER_TIMEOUT: Duration = Duration::from_secs(45);
/// Jeda polling saat antrean kosong.
pub const DEFAULT_POLL: Duration = Duration::from_secs(1);
/// Tidur worker saat Redis bermasalah.
const ERROR_SLEEP: Duration = Duration::from_secs(5);

/// `HSETNX` + `ZADD NX`. KEYS: due, jobs. ARGV: id, json, score. Mengembalikan 1 bila dibuat.
const ENQUEUE_LUA: &str = r"
if redis.call('HSETNX', KEYS[2], ARGV[1], ARGV[2]) == 1 then
  redis.call('ZADD', KEYS[1], 'NX', ARGV[3], ARGV[1])
  return 1
end
return 0
";

/// KEYS: due, jobs, paused, limiter. ARGV: now, lease_until, limit, spacing_ms (0 = tanpa limiter).
/// Mengembalikan `[id1, json1, id2, json2, ...]`; kosong bila antrean dijeda atau limiter aktif.
/// Member `due` tanpa payload (yatim) dibersihkan.
const CLAIM_LUA: &str = r"
if redis.call('EXISTS', KEYS[3]) == 1 then
  return {}
end
local limit = tonumber(ARGV[3])
local spacing = tonumber(ARGV[4])
if spacing > 0 then
  if redis.call('EXISTS', KEYS[4]) == 1 then
    return {}
  end
  limit = 1
end
local ids = redis.call('ZRANGEBYSCORE', KEYS[1], '-inf', ARGV[1], 'LIMIT', 0, limit)
local out = {}
for _, id in ipairs(ids) do
  local p = redis.call('HGET', KEYS[2], id)
  if p then
    redis.call('ZADD', KEYS[1], ARGV[2], id)
    out[#out + 1] = id
    out[#out + 1] = p
  else
    redis.call('ZREM', KEYS[1], id)
  end
end
if spacing > 0 and #out > 0 then
  redis.call('SET', KEYS[4], '1', 'PX', spacing)
end
return out
";

/// Seperti [`ENQUEUE_LUA`] untuk banyak job. KEYS: due, jobs. ARGV: score, lalu pasangan id, json.
/// Mengembalikan jumlah job yang dibuat.
const ENQUEUE_MANY_LUA: &str = r"
local created = 0
for i = 2, #ARGV, 2 do
  if redis.call('HSETNX', KEYS[2], ARGV[i], ARGV[i + 1]) == 1 then
    redis.call('ZADD', KEYS[1], 'NX', ARGV[1], ARGV[i])
    created = created + 1
  end
end
return created
";

/// Hapus job yang MENUNGGU (score <= now; yang sedang di-lease score-nya di masa depan).
/// KEYS: due, jobs. ARGV: now. Mengembalikan jumlah yang dihapus.
const CLEAR_WAITING_LUA: &str = r"
local ids = redis.call('ZRANGEBYSCORE', KEYS[1], '-inf', ARGV[1])
for _, id in ipairs(ids) do
  redis.call('ZREM', KEYS[1], id)
  redis.call('HDEL', KEYS[2], id)
end
return #ids
";

/// KEYS: due, jobs. ARGV: id.
const ACK_LUA: &str = r"
redis.call('ZREM', KEYS[1], ARGV[1])
redis.call('HDEL', KEYS[2], ARGV[1])
return 1
";

/// KEYS: due, jobs. ARGV: id, json baru, score baru. Hanya bila job masih ada.
const RETRY_LUA: &str = r"
if redis.call('HEXISTS', KEYS[2], ARGV[1]) == 1 then
  redis.call('HSET', KEYS[2], ARGV[1], ARGV[2])
  redis.call('ZADD', KEYS[1], ARGV[3], ARGV[1])
  return 1
end
return 0
";

/// Opsi job (setara `attempts` + `backoff: {type: 'exponential', delay}` Bull).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobOpts {
    /// Total percobaan (termasuk yang pertama). Minimal 1.
    pub max_attempts: u32,
    /// Jeda dasar backoff eksponensial.
    pub backoff: Duration,
}

impl JobOpts {
    pub const fn new(max_attempts: u32, backoff: Duration) -> Self {
        Self {
            max_attempts,
            backoff,
        }
    }
}

/// Bentuk job di HASH `mz:q:<name>:jobs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredJob {
    pub data: Value,
    /// Percobaan yang sudah SELESAI (handler pernah mengembalikan Retry).
    pub attempts: u32,
    pub max_attempts: u32,
    pub backoff_ms: u64,
    pub enqueued_at: i64,
}

/// Job yang sedang dipegang worker.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub data: Value,
    /// Nomor percobaan yang sedang berjalan, mulai dari 1.
    pub attempt: u32,
    pub max_attempts: u32,
    pub backoff_ms: u64,
    pub enqueued_at: i64,
}

impl Job {
    fn from_stored(id: String, s: StoredJob) -> Self {
        Self {
            id,
            data: s.data,
            attempt: s.attempts.saturating_add(1),
            max_attempts: s.max_attempts,
            backoff_ms: s.backoff_ms,
            enqueued_at: s.enqueued_at,
        }
    }

    /// Percobaan ini yang terakhir (retry tidak akan dijadwalkan lagi).
    pub fn is_last_attempt(&self) -> bool {
        is_exhausted(self.attempt, self.max_attempts)
    }
}

/// Hasil handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Sukses: job dihapus.
    Done,
    /// Gagal sementara: dijadwalkan ulang dengan backoff (bila jatah percobaan masih ada).
    Retry(String),
    /// Gagal final / tidak perlu diproses: job dihapus tanpa retry. Handler yang mencatat lognya.
    Drop(String),
}

/// Parameter worker. Bawaan: lihat konstanta `DEFAULT_*`.
#[derive(Debug, Clone, Copy)]
pub struct WorkerConfig {
    pub concurrency: usize,
    pub lease: Duration,
    pub handler_timeout: Duration,
    pub poll: Duration,
}

impl WorkerConfig {
    pub const fn new(concurrency: usize) -> Self {
        Self {
            concurrency,
            lease: DEFAULT_LEASE,
            handler_timeout: DEFAULT_HANDLER_TIMEOUT,
            poll: DEFAULT_POLL,
        }
    }
}

// ── Bagian murni ─────────────────────────────────────────────────────────────

/// `backoffMs * 2^(attemptsMade-1)` (Bull `exponential`), saturasi aman.
pub fn backoff_delay_ms(base_ms: u64, attempts_made: u32) -> u64 {
    let exp = attempts_made.saturating_sub(1).min(32);
    base_ms.saturating_mul(1u64 << exp)
}

/// `attempts_made` percobaan sudah memakai seluruh jatah.
pub fn is_exhausted(attempts_made: u32, max_attempts: u32) -> bool {
    attempts_made >= max_attempts.max(1)
}

pub fn due_key(name: &str) -> String {
    format!("mz:q:{name}:due")
}

pub fn jobs_key(name: &str) -> String {
    format!("mz:q:{name}:jobs")
}

/// Penanda jeda antrean (ada = dijeda).
pub fn paused_key(name: &str) -> String {
    format!("mz:q:{name}:paused")
}

/// Penanda limiter jarak mulai (ada = belum boleh klaim lagi; TTL = sisa jeda).
pub fn limiter_key(name: &str) -> String {
    format!("mz:q:{name}:limit")
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn dur_ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

// ── Operasi Redis ────────────────────────────────────────────────────────────

/// Masukkan job. `id = None` -> UUID baru. Id yang sudah ada TIDAK membuat job ganda
/// (job lama dibiarkan apa adanya). Mengembalikan id job.
pub async fn enqueue(
    state: &AppState,
    name: &str,
    id: Option<String>,
    data: Value,
    opts: JobOpts,
) -> anyhow::Result<String> {
    enqueue_delayed(state, name, id, data, opts, Duration::ZERO).await
}

/// [`enqueue`] dengan opsi `delay` Bull: job baru jatuh tempo `now + delay` (format job identik,
/// jadi worker memprosesnya biasa). Id yang sudah ada tidak membuat job ganda dan penundaannya
/// tidak diubah.
pub async fn enqueue_delayed(
    state: &AppState,
    name: &str,
    id: Option<String>,
    data: Value,
    opts: JobOpts,
    delay: Duration,
) -> anyhow::Result<String> {
    let id = id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    anyhow::ensure!(!id.is_empty(), "id job tidak boleh kosong");
    let now = now_ms();
    let stored = StoredJob {
        data,
        attempts: 0,
        max_attempts: opts.max_attempts.max(1),
        backoff_ms: u64::try_from(opts.backoff.as_millis()).unwrap_or(u64::MAX),
        enqueued_at: now,
    };
    let json = serde_json::to_string(&stored)?;
    let due = now.saturating_add(dur_ms(delay));
    let _created: i64 = state
        .redis
        .next()
        .eval(
            ENQUEUE_LUA,
            vec![due_key(name), jobs_key(name)],
            vec![id.clone(), json, due.to_string()],
        )
        .await?;
    Ok(id)
}

/// Klaim paling banyak `limit` job yang sudah jatuh tempo; masing-masing dikunci `lease`.
/// Payload rusak dibuang (log error). Antrean yang dijeda ([`pause`]) mengembalikan kosong.
pub async fn claim(
    state: &AppState,
    name: &str,
    limit: usize,
    lease: Duration,
) -> anyhow::Result<Vec<Job>> {
    claim_limited(state, name, limit, lease, None).await
}

/// Seperti [`claim`] dengan limiter jarak mulai: `spacing = Some(d)` -> paling banyak SATU job
/// per `d` untuk seluruh antrean (kunci Redis bersama, berlaku lintas worker/instance).
pub async fn claim_limited(
    state: &AppState,
    name: &str,
    limit: usize,
    lease: Duration,
    spacing: Option<Duration>,
) -> anyhow::Result<Vec<Job>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let now = now_ms();
    let flat: Vec<String> = state
        .redis
        .next()
        .eval(
            CLAIM_LUA,
            vec![
                due_key(name),
                jobs_key(name),
                paused_key(name),
                limiter_key(name),
            ],
            vec![
                now.to_string(),
                now.saturating_add(dur_ms(lease)).to_string(),
                limit.to_string(),
                spacing.map_or(0, dur_ms).to_string(),
            ],
        )
        .await?;
    let mut jobs = Vec::with_capacity(flat.len() / 2);
    let mut it = flat.into_iter();
    while let (Some(id), Some(raw)) = (it.next(), it.next()) {
        match serde_json::from_str::<StoredJob>(&raw) {
            Ok(s) => jobs.push(Job::from_stored(id, s)),
            Err(e) => {
                tracing::error!(queue = name, job = %id, error = %e, "payload job rusak, dibuang");
                if let Err(e) = ack(state, name, &id).await {
                    tracing::warn!(queue = name, job = %id, error = %e, "gagal membuang job rusak");
                }
            }
        }
    }
    Ok(jobs)
}

/// Masukkan banyak job sekaligus (`(id, data)`, id wajib). Id yang sudah ada dilewati (job lama
/// dibiarkan). Mengembalikan jumlah job yang BENAR-BENAR dibuat.
pub async fn enqueue_many(
    state: &AppState,
    name: &str,
    jobs: Vec<(String, Value)>,
    opts: JobOpts,
) -> anyhow::Result<usize> {
    const CHUNK: usize = 500;
    let now = now_ms();
    let mut created_total = 0usize;
    let mut rest = jobs.into_iter().peekable();
    while rest.peek().is_some() {
        let mut args: Vec<String> = vec![now.to_string()];
        for (id, data) in rest.by_ref().take(CHUNK) {
            anyhow::ensure!(!id.is_empty(), "id job tidak boleh kosong");
            let stored = StoredJob {
                data,
                attempts: 0,
                max_attempts: opts.max_attempts.max(1),
                backoff_ms: u64::try_from(opts.backoff.as_millis()).unwrap_or(u64::MAX),
                enqueued_at: now,
            };
            args.push(id);
            args.push(serde_json::to_string(&stored)?);
        }
        let created: usize = state
            .redis
            .next()
            .eval(ENQUEUE_MANY_LUA, vec![due_key(name), jobs_key(name)], args)
            .await?;
        created_total = created_total.saturating_add(created);
    }
    Ok(created_total)
}

/// Jeda antrean: worker berhenti mengambil job baru (job berjalan selesai normal). Idempoten.
pub async fn pause(state: &AppState, name: &str) -> anyhow::Result<()> {
    let _: () = state
        .redis
        .next()
        .set(paused_key(name), "1", None, None, false)
        .await?;
    Ok(())
}

/// Lanjutkan antrean yang dijeda. Idempoten.
pub async fn resume(state: &AppState, name: &str) -> anyhow::Result<()> {
    let _: i64 = state.redis.next().del(paused_key(name)).await?;
    Ok(())
}

pub async fn is_paused(state: &AppState, name: &str) -> anyhow::Result<bool> {
    let n: i64 = state.redis.next().exists(paused_key(name)).await?;
    Ok(n > 0)
}

/// Hapus semua job yang MENUNGGU (jatuh tempo, belum di-lease); yang sedang berjalan tidak
/// disentuh. Mengembalikan jumlah yang dihapus.
pub async fn clear_waiting(state: &AppState, name: &str) -> anyhow::Result<usize> {
    let n: usize = state
        .redis
        .next()
        .eval(
            CLEAR_WAITING_LUA,
            vec![due_key(name), jobs_key(name)],
            vec![now_ms().to_string()],
        )
        .await?;
    Ok(n)
}

/// Hapus SELURUH keadaan antrean (job menunggu + berjalan, penanda jeda, limiter), setara
/// `Queue.obliterate` Bull. Dipakai uji; jangan dipanggil dari produksi.
pub async fn obliterate(state: &AppState, name: &str) -> anyhow::Result<()> {
    let _: i64 = state
        .redis
        .next()
        .del(vec![
            due_key(name),
            jobs_key(name),
            paused_key(name),
            limiter_key(name),
        ])
        .await?;
    Ok(())
}

/// Jumlah job yang menunggu (jatuh tempo, belum di-lease).
pub async fn waiting_len(state: &AppState, name: &str) -> anyhow::Result<usize> {
    // Epoch ms (~1.8e12) tepat di f64 (mantisa 53 bit).
    #[allow(clippy::cast_precision_loss)]
    let now = now_ms() as f64;
    let n: usize = state
        .redis
        .next()
        .zcount(due_key(name), f64::NEG_INFINITY, now)
        .await?;
    Ok(n)
}

/// Hapus job (sukses atau final).
pub async fn ack(state: &AppState, name: &str, id: &str) -> anyhow::Result<()> {
    let _: i64 = state
        .redis
        .next()
        .eval(
            ACK_LUA,
            vec![due_key(name), jobs_key(name)],
            vec![id.to_owned()],
        )
        .await?;
    Ok(())
}

/// Jadwalkan ulang `job` (percobaan yang baru selesai dihitung) dengan penundaan `delay_ms`.
pub async fn retry(state: &AppState, name: &str, job: &Job, delay_ms: u64) -> anyhow::Result<()> {
    let stored = StoredJob {
        data: job.data.clone(),
        attempts: job.attempt,
        max_attempts: job.max_attempts,
        backoff_ms: job.backoff_ms,
        enqueued_at: job.enqueued_at,
    };
    let due = now_ms().saturating_add(i64::try_from(delay_ms).unwrap_or(i64::MAX));
    let _: i64 = state
        .redis
        .next()
        .eval(
            RETRY_LUA,
            vec![due_key(name), jobs_key(name)],
            vec![
                job.id.clone(),
                serde_json::to_string(&stored)?,
                due.to_string(),
            ],
        )
        .await?;
    Ok(())
}

/// Jumlah job di antrean (menunggu + sedang di-lease).
pub async fn len(state: &AppState, name: &str) -> anyhow::Result<usize> {
    let n: usize = state.redis.next().hlen(jobs_key(name)).await?;
    Ok(n)
}

/// Baca job mentah (untuk diagnosis/test).
pub async fn get(state: &AppState, name: &str, id: &str) -> anyhow::Result<Option<StoredJob>> {
    let raw: Option<String> = state.redis.next().hget(jobs_key(name), id).await?;
    Ok(raw.map(|r| serde_json::from_str(&r)).transpose()?)
}

// ── Worker ───────────────────────────────────────────────────────────────────

/// Worker dengan konfigurasi bawaan (poll 1 dtk, lease 60 dtk, timeout handler 45 dtk).
pub fn spawn_worker<F, Fut>(
    state: &AppState,
    name: &str,
    concurrency: usize,
    handler: F,
) -> JoinHandle<()>
where
    F: Fn(Job) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Outcome> + Send + 'static,
{
    spawn_worker_with(state, name, WorkerConfig::new(concurrency), handler)
}

/// Seperti [`spawn_worker`] dengan [`WorkerConfig`] eksplisit (dipakai test).
pub fn spawn_worker_with<F, Fut>(
    state: &AppState,
    name: &str,
    cfg: WorkerConfig,
    handler: F,
) -> JoinHandle<()>
where
    F: Fn(Job) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Outcome> + Send + 'static,
{
    spawn_worker_limited(state, name, cfg, None, handler)
}

/// Seperti [`spawn_worker_with`] dengan limiter jarak mulai opsional (`Some(d)`: paling banyak
/// satu job dimulai per `d` untuk seluruh antrean; lihat [`claim_limited`]). Jarak nyata antar
/// mulai = `d` sampai `d + cfg.poll`.
pub fn spawn_worker_limited<F, Fut>(
    state: &AppState,
    name: &str,
    cfg: WorkerConfig,
    spacing: Option<Duration>,
    handler: F,
) -> JoinHandle<()>
where
    F: Fn(Job) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Outcome> + Send + 'static,
{
    let state = state.clone();
    let name = name.to_owned();
    let handler = Arc::new(handler);
    tokio::spawn(async move {
        let concurrency = cfg.concurrency.max(1);
        let sem = Arc::new(Semaphore::new(concurrency));
        loop {
            // Tunggu minimal satu slot, lalu klaim sebanyak slot yang bebas.
            let Ok(first) = Arc::clone(&sem).acquire_owned().await else {
                return;
            };
            let limit = sem.available_permits().saturating_add(1).min(concurrency);
            let jobs = match claim_limited(&state, &name, limit, cfg.lease, spacing).await {
                Ok(j) => j,
                Err(e) => {
                    tracing::error!(queue = %name, error = %e, "worker: gagal klaim job; coba lagi nanti");
                    drop(first);
                    tokio::time::sleep(ERROR_SLEEP).await;
                    continue;
                }
            };
            if jobs.is_empty() {
                drop(first);
                tokio::time::sleep(cfg.poll).await;
                continue;
            }
            let batch_full = jobs.len() >= limit;
            let mut first = Some(first);
            for job in jobs {
                let permit = match first.take() {
                    Some(p) => p,
                    None => match Arc::clone(&sem).acquire_owned().await {
                        Ok(p) => p,
                        Err(_) => return,
                    },
                };
                let state = state.clone();
                let name = name.clone();
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    run_job(&state, &name, job, cfg.handler_timeout, handler.as_ref()).await;
                    drop(permit);
                });
            }
            if !batch_full {
                tokio::time::sleep(cfg.poll).await;
            }
        }
    })
}

/// Jalankan handler (di task terpisah supaya panic tertangkap, dibatasi timeout) lalu ack/retry.
async fn run_job<F, Fut>(state: &AppState, name: &str, job: Job, timeout: Duration, handler: &F)
where
    F: Fn(Job) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Outcome> + Send + 'static,
{
    let mut task = tokio::spawn(handler(job.clone()));
    let outcome = match tokio::time::timeout(timeout, &mut task).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => Outcome::Retry(format!("handler panic: {e}")),
        Err(_) => {
            task.abort();
            Outcome::Retry(format!("handler timeout setelah {timeout:?}"))
        }
    };
    finish(state, name, &job, outcome).await;
}

async fn finish(state: &AppState, name: &str, job: &Job, outcome: Outcome) {
    let res = match outcome {
        Outcome::Done => ack(state, name, &job.id).await,
        Outcome::Drop(reason) => {
            tracing::debug!(queue = name, job = %job.id, %reason, "job dibuang");
            ack(state, name, &job.id).await
        }
        Outcome::Retry(reason) if job.is_last_attempt() => {
            tracing::error!(
                queue = name,
                job = %job.id,
                attempts = job.attempt,
                %reason,
                "job dibuang: percobaan habis"
            );
            ack(state, name, &job.id).await
        }
        Outcome::Retry(reason) => {
            let delay = backoff_delay_ms(job.backoff_ms, job.attempt);
            tracing::warn!(
                queue = name,
                job = %job.id,
                attempt = job.attempt,
                max_attempts = job.max_attempts,
                delay_ms = delay,
                %reason,
                "job dicoba ulang"
            );
            retry(state, name, job, delay).await
        }
    };
    if let Err(e) = res {
        // Lease akan habis dan job diambil lagi (at-least-once).
        tracing::error!(queue = name, job = %job.id, error = %e, "gagal ack/retry job");
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn backoff_eksponensial_ala_bull() {
        assert_eq!(backoff_delay_ms(30_000, 1), 30_000);
        assert_eq!(backoff_delay_ms(30_000, 2), 60_000);
        assert_eq!(backoff_delay_ms(30_000, 3), 120_000);
        assert_eq!(backoff_delay_ms(30_000, 4), 240_000);
        assert_eq!(backoff_delay_ms(30_000, 0), 30_000);
        assert_eq!(backoff_delay_ms(0, 5), 0);
        assert_eq!(backoff_delay_ms(u64::MAX, 40), u64::MAX);
    }

    #[test]
    fn jatah_percobaan() {
        assert!(!is_exhausted(1, 5));
        assert!(!is_exhausted(4, 5));
        assert!(is_exhausted(5, 5));
        assert!(is_exhausted(1, 0));
        assert!(is_exhausted(1, 1));
    }

    #[test]
    fn kunci_redis() {
        assert_eq!(due_key("mixpanel"), "mz:q:mixpanel:due");
        assert_eq!(jobs_key("mixpanel"), "mz:q:mixpanel:jobs");
        assert_eq!(paused_key("screenshot"), "mz:q:screenshot:paused");
        assert_eq!(limiter_key("screenshot"), "mz:q:screenshot:limit");
    }

    #[test]
    fn serialisasi_job_camel_case() {
        let s = StoredJob {
            data: json!({"a": [1, 2], "kosong": []}),
            attempts: 2,
            max_attempts: 5,
            backoff_ms: 30_000,
            enqueued_at: 1_700_000_000_000,
        };
        let v = serde_json::to_value(&s).unwrap_or_default();
        assert_eq!(v["maxAttempts"], 5);
        assert_eq!(v["backoffMs"], 30_000);
        assert_eq!(v["enqueuedAt"], 1_700_000_000_000_i64);
        assert_eq!(v["data"]["kosong"], json!([]));
        let back: Result<StoredJob, _> = serde_json::from_str(&v.to_string());
        assert_eq!(back.ok(), Some(s));
    }

    #[test]
    fn job_dari_stored_menghitung_percobaan_berikutnya() {
        let s = StoredJob {
            data: json!(1),
            attempts: 4,
            max_attempts: 5,
            backoff_ms: 10,
            enqueued_at: 0,
        };
        let job = Job::from_stored("x".into(), s);
        assert_eq!(job.attempt, 5);
        assert!(job.is_last_attempt());
    }
}
