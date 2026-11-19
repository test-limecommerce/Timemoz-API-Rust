//! Kerja latar screenshot: retention purge 02:15, enqueue compress 02:45, enqueue purge
//! permanen 03:40 (semua `Asia/Jakarta`) + worker antrean `screenshot`.
//!
//! Node: `screenshots/screenshots.cron.ts` + `ScreenshotService.purgeExpiredScreenshots`,
//! `tasks/tasks.service.ts` (`enqueueScreenshotCompress`, `enqueueScreenshotPurge`),
//! `tracking-timelines/screenshot.consumer.ts`, `admin/admin.controller.ts` (produsen manual).
//!
//! Antrean `screenshot` = `mz_core::queue` (Redis `mz:q:screenshot:*`, persisten, at-least-once;
//! terpisah dari Bull Node). Cron dan route admin hanya MENG-ENQUEUE ([`enqueue_compress`],
//! [`enqueue_purge`]); [`spawn_worker`] (dipasang `mz-server`) yang mengerjakan:
//! - limiter Bull `{max: 1, duration: 3000}` = [`QUEUE_LIMITER_SPACING`] (global di Redis),
//! - `@Process` compress concurrency 1 + purge concurrency 2 (Bull menjumlahkannya: 3 slot untuk
//!   antrean; kuota per jenis ditegakkan semaphore per proses),
//! - `timeout` job [`SCREENSHOT_JOB_TIMEOUT`] (2 menit), `attempts: 1` (tanpa retry),
//! - jeda/lanjut/kosongkan lewat `mz_core::queue::{pause, resume, clear_waiting}`
//!   (route `admin/screenshot/queue*` di `mz-timeconfig`).
//!
//! Job idempoten dan ber-id deterministik (`screenshot.<jenis>:<id>`): enqueue ulang id yang
//! masih menunggu/berjalan tidak membuat job ganda; job yang dipulihkan sesudah proses mati
//! aman dijalankan ulang.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use chrono::{Duration as ChronoDuration, NaiveDateTime};
use futures::stream::{self, StreamExt};
use mz_core::AppState;
use mz_core::queue::{self, Job, JobOpts, Outcome, WorkerConfig};
use mz_core::time::now_naive;
use mz_jobs::{CronJob, Schedule};
use serde_json::json;
use tokio::sync::Semaphore;

use crate::screenshots::access::{object_key_or_null, strip_blur_suffix};

/// `ScreenshotEntity.STATUS_DELETED`.
pub const STATUS_DELETED: i32 = 0;
/// `ScreenshotEntity.STATUS_FINISHED`.
pub const STATUS_FINISHED: i32 = 3;
/// `PURGE_BATCH_SIZE` (purgeExpiredScreenshots).
const PURGE_BATCH_SIZE: i64 = 1000;
/// `PURGE_DELETE_CONCURRENCY`.
const PURGE_DELETE_CONCURRENCY: usize = 16;
/// `take` enqueueScreenshotCompress.
const COMPRESS_TAKE: i64 = 200;
/// `take` per grup retensi enqueueScreenshotPurge.
const PURGE_ROWS_TAKE: i64 = 5000;
/// `SCREENSHOT_JOB_TIMEOUT_MS`.
pub const SCREENSHOT_JOB_TIMEOUT: Duration = Duration::from_secs(120);
/// Limiter antrean `screenshot`: max 1 job / 3000 ms.
pub const QUEUE_LIMITER_SPACING: Duration = Duration::from_secs(3);
/// Nama antrean `mz_core::queue` (kunci Redis `mz:q:screenshot:*`).
pub const QUEUE: &str = "screenshot";
/// `concurrency` consumer per jenis job (`@Process({ name, concurrency })`).
pub const COMPRESS_CONCURRENCY: usize = 1;
pub const PURGE_CONCURRENCY: usize = 2;
/// Batas handler di level antrean (menunggu kuota jenis + [`SCREENSHOT_JOB_TIMEOUT`] kerja);
/// lease = ini + 1 menit, jadi job milik proses yang mati diambil lagi sekitar 11 menit kemudian.
const HANDLER_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const LEASE: Duration = Duration::from_secs(11 * 60);
/// Jeda polling worker; jarak mulai antar job nyata = 3 dtk sampai 3 dtk + ini.
const POLL: Duration = Duration::from_millis(500);
/// Parameter kompresi consumer: `image.blur(50)` (Jimp, radius) ~ sigma 25 di `fast_blur`;
/// JPEG kualitas 65.
pub const COMPRESS_BLUR_SIGMA: f32 = 25.0;
pub const COMPRESS_JPEG_QUALITY: u8 = 65;

/// Job cron yang harus didaftarkan orkestrator.
pub fn cron_jobs() -> Vec<CronJob> {
    vec![
        CronJob {
            name: "screenshot-purge",
            // Node: screenshots/screenshots.cron.ts:15 `@Cron('15 2 * * *', WIB)`
            schedule: Schedule::daily(&[2], &[15]),
            timeout: Duration::from_secs(2 * 3600),
            run: |s| Box::pin(run_retention_purge(s)),
        },
        CronJob {
            name: "screenshot-compress",
            // Node: tasks/tasks.service.ts:3274 `@Cron('45 2 * * *', WIB)`
            schedule: Schedule::daily(&[2], &[45]),
            // Hanya SELECT + enqueue (pekerjaan berat ada di worker antrean).
            timeout: Duration::from_secs(10 * 60),
            run: |s| Box::pin(run_compress(s)),
        },
        CronJob {
            name: "screenshot-purge-deleted",
            // Node: tasks/tasks.service.ts:3316 `@Cron('40 3 * * *', WIB)`
            schedule: Schedule::daily(&[3], &[40]),
            // Hanya SELECT + enqueue; backlog dikerjakan worker antrean (persisten di Redis).
            timeout: Duration::from_secs(10 * 60),
            run: |s| Box::pin(run_purge_deleted(s)),
        },
    ]
}

// ---------------------------------------------------------------- logika murni

/// `retention === null || <= 0 || >= 36500`.
pub fn is_unlimited(days: Option<i32>) -> bool {
    days.is_none_or(|d| d <= 0 || d >= 36500)
}

/// "Free tier" menurut consumer compress: `0 < days < 36500`.
pub fn is_free_retention(days: Option<i32>) -> bool {
    matches!(days, Some(d) if d > 0 && d < 36500)
}

/// `setDate(getDate() - days)` (zona UTC; createdAt = `timestamp(3)` UTC).
pub fn cutoff(now: NaiveDateTime, days: i32) -> NaiveDateTime {
    now - ChronoDuration::days(i64::from(days))
}

/// projectId -> retensi maksimum seluruh company aktif yang tertaut; satu company
/// unlimited (atau company tidak aktif/tak dikenal) melindungi project. Hasil: hari -> projectIds
/// (hanya project yang boleh dipurge), terurut.
pub fn retention_groups(
    companies: &[(i32, Option<i32>)],
    links: &[(i32, i32)],
) -> BTreeMap<i32, Vec<i32>> {
    let retention: HashMap<i32, Option<i32>> = companies.iter().copied().collect();
    // None = Infinity.
    let mut per_project: BTreeMap<i32, Option<i32>> = BTreeMap::new();
    for &(project_id, company_id) in links {
        let r = retention.get(&company_id).copied().flatten();
        if is_unlimited(r) {
            per_project.insert(project_id, None);
            continue;
        }
        let cur = per_project.entry(project_id).or_insert(Some(0));
        if let (Some(c), Some(r)) = (*cur, r) {
            *cur = Some(c.max(r));
        }
    }
    let mut groups: BTreeMap<i32, Vec<i32>> = BTreeMap::new();
    for (project_id, days) in per_project {
        if let Some(d) = days.filter(|d| *d > 0) {
            groups.entry(d).or_default().push(project_id);
        }
    }
    groups
}

/// `ScreenshotConsumer.isPastUserRetention`: retensi MAKS di semua company user; satu yang
/// unlimited melindungi. Tanpa company -> `false`.
pub fn is_past_user_retention(
    created_at: NaiveDateTime,
    retentions: &[Option<i32>],
    now: NaiveDateTime,
) -> bool {
    let mut max = 0;
    for &r in retentions {
        if is_unlimited(r) {
            return false;
        }
        max = max.max(r.unwrap_or(0));
    }
    if max <= 0 {
        return false;
    }
    created_at <= cutoff(now, max)
}

/// Key object hasil kompres: `compressed/${baseKey}-${Date.now()}.${ext}` dengan
/// `ext = key.split('.').pop()` dan `baseKey = key.replace(`.${ext}`, '')` (kemunculan pertama).
pub fn compressed_key(file_key: &str, now_ms: i64) -> String {
    let ext = file_key.rsplit('.').next().unwrap_or(file_key);
    let base = file_key.replacen(&format!(".{ext}"), "", 1);
    format!("compressed/{base}-{now_ms}.{ext}")
}

/// Keys object yang dimiliki screenshot terpurge (`deleteScreenshotObjects`), berurutan tanpa duplikat.
pub fn objects_to_delete(
    bucket: Option<&str>,
    file_fullpath: &str,
    is_blur: bool,
    compressed_key: Option<&str>,
) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    let mut add = |k: String| {
        if !keys.contains(&k) {
            keys.push(k);
        }
    };
    if let Some(k) = object_key_or_null(bucket, Some(file_fullpath)) {
        if is_blur {
            add(k.clone());
            add(strip_blur_suffix(&k));
        } else {
            add(k);
        }
    }
    if let Some(k) = object_key_or_null(bucket, compressed_key) {
        add(k);
    }
    keys
}

/// Olah gambar consumer compress (CPU): decode -> blur -> JPEG. `Err(alasan)` = tak bisa dikompres.
pub fn compress_image(original: &[u8]) -> Result<Vec<u8>, String> {
    if original.is_empty() {
        return Err("object is empty (0 bytes)".to_owned());
    }
    let img = image::load_from_memory(original)
        .map_err(|e| format!("cannot decode image ({} bytes): {e}", original.len()))?
        .to_rgb8();
    let blurred = image::imageops::fast_blur(&img, COMPRESS_BLUR_SIGMA);
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, COMPRESS_JPEG_QUALITY)
        .encode_image(&image::DynamicImage::ImageRgb8(blurred))
        .map_err(|e| format!("jpeg encode failed: {e}"))?;
    Ok(out)
}

// ---------------------------------------------------------------- antrean

/// Jenis job (`screenshot.compress` / `screenshot.purge` di Node).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Compress,
    Purge,
}

impl Kind {
    /// Nama job Bull (`job.name`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Compress => "screenshot.compress",
            Self::Purge => "screenshot.purge",
        }
    }

    fn short(self) -> &'static str {
        match self {
            Self::Compress => "compress",
            Self::Purge => "purge",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "compress" => Some(Self::Compress),
            "purge" => Some(Self::Purge),
            _ => None,
        }
    }

    /// Id job deterministik (`jobId` Node `screenshot.purge:<id>`).
    pub fn job_id(self, screenshot_id: i32) -> String {
        format!("{}:{screenshot_id}", self.name())
    }
}

/// Opsi job: `attempts: 1` (default global Bull) = tanpa retry.
const JOB_OPTS: JobOpts = JobOpts::new(1, Duration::ZERO);

/// Masukkan job compress untuk `ids`. Mengembalikan jumlah job BARU (id yang sudah menunggu/berjalan
/// dilewati).
pub async fn enqueue_compress(state: &AppState, ids: &[i32]) -> anyhow::Result<usize> {
    enqueue_kind(state, Kind::Compress, ids).await
}

/// Masukkan job purge untuk `ids` (lihat [`enqueue_compress`]).
pub async fn enqueue_purge(state: &AppState, ids: &[i32]) -> anyhow::Result<usize> {
    enqueue_kind(state, Kind::Purge, ids).await
}

async fn enqueue_kind(state: &AppState, kind: Kind, ids: &[i32]) -> anyhow::Result<usize> {
    let jobs = ids
        .iter()
        .map(|&id| {
            (
                kind.job_id(id),
                json!({ "kind": kind.short(), "screenshotId": id }),
            )
        })
        .collect();
    queue::enqueue_many(state, QUEUE, jobs, JOB_OPTS).await
}

/// Kuota per jenis job (concurrency `@Process` Node), per proses.
pub struct Limits {
    compress: Semaphore,
    purge: Semaphore,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            compress: Semaphore::new(COMPRESS_CONCURRENCY),
            purge: Semaphore::new(PURGE_CONCURRENCY),
        }
    }
}

/// Satu job antrean: kuota jenis -> kerja dibatasi [`SCREENSHOT_JOB_TIMEOUT`]. Galat/timeout =
/// `Retry` yang, karena `attempts: 1`, berakhir sebagai job dibuang + log error (Node: job gagal
/// lalu `removeOnFail`).
pub async fn handle_job(state: &AppState, limits: &Limits, job: Job) -> Outcome {
    let parsed = job
        .data
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .and_then(Kind::parse)
        .zip(
            job.data
                .get("screenshotId")
                .and_then(serde_json::Value::as_i64)
                .and_then(|n| i32::try_from(n).ok()),
        );
    let Some((kind, id)) = parsed else {
        return Outcome::Drop(format!("payload job screenshot tidak valid: {}", job.data));
    };
    let sem = match kind {
        Kind::Compress => &limits.compress,
        Kind::Purge => &limits.purge,
    };
    let Ok(_permit) = sem.acquire().await else {
        return Outcome::Retry("semaphore ditutup".to_owned());
    };
    let work = async {
        match kind {
            Kind::Compress => compress_screenshot(state, id).await,
            Kind::Purge => purge_screenshot(state, id).await,
        }
    };
    match tokio::time::timeout(SCREENSHOT_JOB_TIMEOUT, work).await {
        Ok(Ok(())) => Outcome::Done,
        Ok(Err(e)) => Outcome::Retry(format!("{} {id}: {e:#}", kind.name())),
        Err(_) => Outcome::Retry(format!(
            "{} {id}: timeout setelah {SCREENSHOT_JOB_TIMEOUT:?}",
            kind.name()
        )),
    }
}

/// Pasang worker antrean `screenshot`. Mati bila tugasnya di-abort; job yang terputus diambil lagi
/// sesudah lease habis (at-least-once).
pub fn spawn_worker(state: &AppState) -> tokio::task::JoinHandle<()> {
    let st = state.clone();
    let limits = Arc::new(Limits::default());
    let cfg = WorkerConfig {
        concurrency: COMPRESS_CONCURRENCY + PURGE_CONCURRENCY,
        lease: LEASE,
        handler_timeout: HANDLER_TIMEOUT,
        poll: POLL,
    };
    queue::spawn_worker_limited(state, QUEUE, cfg, Some(QUEUE_LIMITER_SPACING), move |job| {
        let st = st.clone();
        let limits = Arc::clone(&limits);
        async move { handle_job(&st, &limits, job).await }
    })
}

fn bucket(state: &AppState) -> Option<String> {
    state.config.var("GCP_BUCKET_NAME")
}

// ---------------------------------------------------------------- 02:15 retention purge

/// Node: `ScreenshotCron.handleDailyPurge` -> `purgeExpiredScreenshots`.
pub async fn run_retention_purge(state: AppState) -> anyhow::Result<()> {
    tracing::info!("Running screenshot retention purge...");
    match purge_expired_screenshots(&state).await {
        Ok(n) => {
            tracing::info!("Screenshot purge completed. Deleted: {n} screenshots");
            Ok(())
        }
        Err(e) => {
            tracing::error!(error = %e, "Screenshot purge failed");
            Ok(())
        }
    }
}

/// Kelompok retensi (companies aktif + projectCompany status<>0) -> `{hari -> projectIds}`.
async fn load_retention_groups(state: &AppState) -> anyhow::Result<BTreeMap<i32, Vec<i32>>> {
    let companies: Vec<(i32, Option<i32>)> =
        sqlx::query_as(r#"SELECT id, "screenshotRetentionDays" FROM company WHERE status = 1"#)
            .fetch_all(&state.db)
            .await?;
    let links: Vec<(i32, i32)> = sqlx::query_as(
        r#"SELECT "projectId", "companyId" FROM "projectCompany" WHERE status <> 0"#,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(retention_groups(&companies, &links))
}

/// `purgeExpiredScreenshots`: tandai DELETED dulu, lalu hapus object (urutan Node). Mengembalikan
/// jumlah baris yang ditandai.
pub async fn purge_expired_screenshots(state: &AppState) -> anyhow::Result<u64> {
    let mut total = 0u64;
    let bucket = bucket(state);
    for (days, project_ids) in load_retention_groups(state).await? {
        let cutoff_date = cutoff(now_naive(), days);
        let mut last_id = 0i32;
        loop {
            let batch: Vec<(i32, String, String, bool, Option<String>)> = sqlx::query_as(
                r#"SELECT id, uuid::text, "fileFullpath", "isBlur", "compressedFileKey"
                   FROM screenshot
                   WHERE "createdAt" < $1 AND status <> $2 AND "projectId" = ANY($3) AND id > $4
                   ORDER BY id ASC LIMIT $5"#,
            )
            .bind(cutoff_date)
            .bind(STATUS_DELETED)
            .bind(&project_ids)
            .bind(last_id)
            .bind(PURGE_BATCH_SIZE)
            .fetch_all(&state.db)
            .await?;
            let Some(last) = batch.last() else { break };
            last_id = last.0;
            let ids: Vec<i32> = batch.iter().map(|r| r.0).collect();

            // Tandai DELETED SEBELUM objek dihapus; predikat diulang (TOCTOU seperti Node).
            let updated = sqlx::query(
                r#"UPDATE screenshot SET status = $1, "updatedAt" = $2
                   WHERE "createdAt" < $3 AND status <> $1 AND "projectId" = ANY($4) AND id = ANY($5)"#,
            )
            .bind(STATUS_DELETED)
            .bind(now_naive())
            .bind(cutoff_date)
            .bind(&project_ids)
            .bind(&ids)
            .execute(&state.db)
            .await?;
            total += updated.rows_affected();

            let n = batch.len();
            stream::iter(batch)
                .map(|(_, uuid, full, is_blur, compressed)| {
                    delete_screenshot_objects(
                        state,
                        bucket.clone(),
                        uuid,
                        full,
                        is_blur,
                        compressed,
                    )
                })
                .buffer_unordered(PURGE_DELETE_CONCURRENCY)
                .collect::<Vec<()>>()
                .await;

            if (n as i64) < PURGE_BATCH_SIZE {
                break;
            }
        }
    }
    Ok(total)
}

/// `deleteScreenshotObjects`: tidak pernah gagal; galat per key hanya dilog.
async fn delete_screenshot_objects(
    state: &AppState,
    bucket: Option<String>,
    uuid: String,
    full: String,
    is_blur: bool,
    compressed: Option<String>,
) {
    for key in objects_to_delete(bucket.as_deref(), &full, is_blur, compressed.as_deref()) {
        if let Err(e) = mz_files::delete_file(state, &key).await {
            tracing::warn!("Failed to delete screenshot object {key} (screenshot {uuid}): {e}");
        }
    }
}

// ---------------------------------------------------------------- 02:45 compress

/// Node: `TasksService.enqueueScreenshotCompress` (pekerjaannya di [`spawn_worker`]).
pub async fn run_compress(state: AppState) -> anyhow::Result<()> {
    let ids = select_compress_candidates(&state, now_naive()).await?;
    if ids.is_empty() {
        return Ok(());
    }
    enqueue_compress(&state, &ids).await?;
    tracing::info!("Enqueued {} screenshots for compression", ids.len());
    Ok(())
}

/// Kueri pemilihan `enqueueScreenshotCompress` (7 hari, free-tier, take 200).
pub async fn select_compress_candidates(
    state: &AppState,
    now: NaiveDateTime,
) -> anyhow::Result<Vec<i32>> {
    let seven_days_ago = now - ChronoDuration::days(7);
    let rows: Vec<(i32,)> = sqlx::query_as(
        r#"SELECT s.id FROM screenshot s
           WHERE s."isCompressed" = false AND s."compressedFileKey" IS NULL
             AND s."compressedAt" IS NULL AND s.status = $1 AND s."fileFullpath" <> ''
             AND s."createdAt" < $2
             AND EXISTS (SELECT 1 FROM "projectCompany" pc JOIN company c ON c.id = pc."companyId"
                         WHERE pc."projectId" = s."projectId"
                           AND c."screenshotRetentionDays" > 0 AND c."screenshotRetentionDays" < 36500)
           ORDER BY s.id ASC LIMIT $3"#,
    )
    .bind(STATUS_FINISHED)
    .bind(seven_days_ago)
    .bind(COMPRESS_TAKE)
    .fetch_all(&state.db)
    .await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

/// Retensi company milik user pemilik screenshot (semua `userCompany`, tanpa filter status).
async fn user_retentions(state: &AppState, screenshot_id: i32) -> sqlx::Result<Vec<Option<i32>>> {
    let rows: Vec<(Option<i32>,)> = sqlx::query_as(
        r#"SELECT c."screenshotRetentionDays"
           FROM screenshot s
           JOIN "trackingTimeline" tt ON tt.id = s."trackingTimelineId"
           JOIN tracking t ON t.id = tt."trackingId"
           JOIN "userCompany" uc ON uc."userId" = t."userId"
           JOIN company c ON c.id = uc."companyId"
           WHERE s.id = $1"#,
    )
    .bind(screenshot_id)
    .fetch_all(&state.db)
    .await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

/// `markUncompressible`: bersyarat agar tak menimpa hasil kompres sah.
pub async fn mark_uncompressible(state: &AppState, id: i32) -> sqlx::Result<()> {
    sqlx::query(
        r#"UPDATE screenshot SET "compressedAt" = $2
           WHERE id = $1 AND "isCompressed" = false AND "compressedAt" IS NULL"#,
    )
    .bind(id)
    .bind(now_naive())
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `ScreenshotConsumer.handleCompress`. Galat = job gagal (Bull attempts 1: tanpa retry).
pub async fn compress_screenshot(state: &AppState, id: i32) -> anyhow::Result<()> {
    let row: Option<(String, bool)> =
        sqlx::query_as(r#"SELECT "fileFullpath", "isCompressed" FROM screenshot WHERE id = $1"#)
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    let Some((full, is_compressed)) = row else {
        return Ok(());
    };
    if is_compressed {
        return Ok(());
    }
    let retentions = user_retentions(state, id).await?;
    if !retentions.iter().any(|r| is_free_retention(*r)) {
        mark_uncompressible(state, id).await?;
        return Ok(());
    }
    let file_key = match mz_files::object_key_of(state, &full) {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!("Screenshot {id} skipped: {e}");
            mark_uncompressible(state, id).await?;
            return Ok(());
        }
    };

    let original = match mz_files::download_buffer(state, &file_key).await {
        Ok(b) => b,
        Err(mz_files::GcsError::NotFound) => {
            tracing::warn!("Screenshot {id} not found in GCS, skipping compression");
            mark_uncompressible(state, id).await?;
            return Ok(());
        }
        Err(e) => {
            tracing::error!("Failed to compress {id}: {e}");
            return Err(anyhow::anyhow!("{e}"));
        }
    };
    let compressed = tokio::task::spawn_blocking(move || compress_image(&original)).await?;
    let compressed = match compressed {
        Ok(b) => b,
        Err(reason) => {
            tracing::warn!("Screenshot {id} skipped: {reason}");
            mark_uncompressible(state, id).await?;
            return Ok(());
        }
    };

    let key = compressed_key(&file_key, chrono::Utc::now().timestamp_millis());
    if let Err(e) = mz_files::upload_buffer(state, &key, compressed, "image/jpeg").await {
        tracing::error!("Failed to compress {id}: {e}");
        return Err(anyhow::anyhow!("{e}"));
    }
    sqlx::query(
        r#"UPDATE screenshot SET "isCompressed" = true, "compressedFileKey" = $2, "compressedAt" = $3
           WHERE id = $1"#,
    )
    .bind(id)
    .bind(&key)
    .bind(now_naive())
    .execute(&state.db)
    .await?;
    tracing::info!("Compressed screenshot {id}");
    Ok(())
}

// ---------------------------------------------------------------- 03:40 purge permanen

/// Node: `TasksService.enqueueScreenshotPurge` (pekerjaannya di [`spawn_worker`]).
pub async fn run_purge_deleted(state: AppState) -> anyhow::Result<()> {
    let ids = select_purge_candidates(&state, now_naive()).await?;
    if ids.is_empty() {
        return Ok(());
    }
    enqueue_purge(&state, &ids).await?;
    tracing::info!("Enqueued {} screenshots for purge", ids.len());
    Ok(())
}

/// Kueri pemilihan `enqueueScreenshotPurge` (baris DELETED lewat retensi project, take 5000/grup).
pub async fn select_purge_candidates(
    state: &AppState,
    now: NaiveDateTime,
) -> anyhow::Result<Vec<i32>> {
    let mut all = Vec::new();
    for (days, project_ids) in load_retention_groups(state).await? {
        let rows: Vec<(i32,)> = sqlx::query_as(
            r#"SELECT id FROM screenshot
               WHERE "createdAt" < $1 AND "projectId" = ANY($2) AND status = $3
               ORDER BY id ASC LIMIT $4"#,
        )
        .bind(cutoff(now, days))
        .bind(&project_ids)
        .bind(STATUS_DELETED)
        .bind(PURGE_ROWS_TAKE)
        .fetch_all(&state.db)
        .await?;
        all.extend(rows.into_iter().map(|r| r.0));
    }
    Ok(all)
}

/// `ScreenshotConsumer.handlePurge`: hapus original, compressed, lalu baris DB. Galat = job gagal
/// (baris tetap; run berikutnya memilihnya lagi).
pub async fn purge_screenshot(state: &AppState, id: i32) -> anyhow::Result<()> {
    let row: Option<(String, Option<String>, NaiveDateTime, i32)> = sqlx::query_as(
        r#"SELECT "fileFullpath", "compressedFileKey", "createdAt", status FROM screenshot WHERE id = $1"#,
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    let Some((full, compressed, created_at, status)) = row else {
        return Ok(());
    };

    if status != STATUS_DELETED {
        let retentions = user_retentions(state, id).await?;
        if !is_past_user_retention(created_at, &retentions, now_naive()) {
            return Ok(());
        }
    }

    // Original: key tak sah -> dilewati (log bila fileFullpath tidak kosong); 404 = sudah tercapai.
    match mz_files::object_key_of(state, &full) {
        Ok(key) => delete_ignoring_missing(state, &key).await?,
        Err(e) if !full.is_empty() => {
            tracing::warn!("Screenshot {id}: not deleting file ({e})");
        }
        Err(_) => {}
    }
    if let Some(ck) = compressed.filter(|k| !k.is_empty()) {
        delete_ignoring_missing(state, &ck).await?;
    }
    sqlx::query("DELETE FROM screenshot WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    tracing::info!("Purged screenshot {id}");
    Ok(())
}

async fn delete_ignoring_missing(state: &AppState, key: &str) -> anyhow::Result<()> {
    match mz_files::delete_file(state, key).await {
        Ok(_) | Err(mz_files::GcsError::NotFound) => Ok(()),
        Err(e) => {
            tracing::error!("Failed to delete object {key}: {e}");
            Err(anyhow::anyhow!("{e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn dt(y: i32, m: u32, d: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
    }

    #[test]
    fn unlimited_rules() {
        assert!(is_unlimited(None));
        assert!(is_unlimited(Some(0)));
        assert!(is_unlimited(Some(-1)));
        assert!(is_unlimited(Some(36500)));
        assert!(!is_unlimited(Some(7)));
        assert!(is_free_retention(Some(7)));
        assert!(!is_free_retention(Some(36500)));
        assert!(!is_free_retention(Some(0)));
        assert!(!is_free_retention(None));
    }

    #[test]
    fn groups_use_max_and_protect_on_unlimited() {
        let companies = [(1, Some(7)), (2, Some(30)), (3, Some(36500)), (4, Some(7))];
        // project 10: 7 & 30 -> 30; project 11: 7 & unlimited -> dilindungi;
        // project 12: company tak dikenal -> dilindungi; project 13: 7; project 14: 7.
        let links = [
            (10, 1),
            (10, 2),
            (11, 1),
            (11, 3),
            (12, 99),
            (13, 4),
            (14, 1),
        ];
        let g = retention_groups(&companies, &links);
        assert_eq!(g.get(&30), Some(&vec![10]));
        assert_eq!(g.get(&7), Some(&vec![13, 14]));
        assert_eq!(g.len(), 2);
        // Unlimited tetap menang walau finite datang sesudahnya.
        let g = retention_groups(&companies, &[(11, 3), (11, 1)]);
        assert!(g.is_empty());
    }

    #[test]
    fn user_retention_cutoff() {
        let now = dt(2026, 11, 16);
        // 30 hari: tepat di cutoff = kedaluwarsa (<=), sehari lebih muda = tidak.
        assert!(is_past_user_retention(dt(2026, 10, 17), &[Some(30)], now));
        assert!(!is_past_user_retention(dt(2026, 10, 18), &[Some(30)], now));
        // Maks lintas company.
        assert!(!is_past_user_retention(
            dt(2026, 10, 17),
            &[Some(30), Some(90)],
            now
        ));
        // Unlimited / tanpa company melindungi.
        assert!(!is_past_user_retention(
            dt(2020, 1, 1),
            &[Some(7), None],
            now
        ));
        assert!(!is_past_user_retention(dt(2020, 1, 1), &[], now));
    }

    #[test]
    fn compressed_key_shape() {
        assert_eq!(
            compressed_key("companies/c/p/u/screenshots/x.jpg", 1700000000000),
            "compressed/companies/c/p/u/screenshots/x-1700000000000.jpg"
        );
        // replace pertama saja, seperti String.replace.
        assert_eq!(compressed_key("a.jpg/b.jpg", 5), "compressed/a/b.jpg-5.jpg");
        assert_eq!(compressed_key("noext", 5), "compressed/noext-5.noext");
    }

    #[test]
    fn objects_to_delete_keys() {
        let k = objects_to_delete(
            Some("b"),
            "companies/c/x-blurred-1700000000000.jpg",
            true,
            Some("compressed/companies/c/x-blurred-1700000000000-9.jpg"),
        );
        assert_eq!(
            k,
            vec![
                "companies/c/x-blurred-1700000000000.jpg",
                "companies/c/x.jpg",
                "compressed/companies/c/x-blurred-1700000000000-9.jpg"
            ]
        );
        let k = objects_to_delete(Some("b"), "companies/c/x.jpg", false, None);
        assert_eq!(k, vec!["companies/c/x.jpg"]);
        assert!(objects_to_delete(Some("b"), "", true, None).is_empty());
    }

    #[test]
    fn compress_image_params() {
        assert!(compress_image(&[]).unwrap_err().contains("empty"));
        assert!(compress_image(b"not an image").is_err());
        // PNG 64x64 bergradasi -> JPEG, lebih kecil dimensinya tetap sama.
        let mut img = image::RgbImage::new(64, 64);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgb([(x * 4) as u8, (y * 4) as u8, 128]);
        }
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let out = compress_image(&png).unwrap();
        assert_eq!(&out[..3], &[0xFF, 0xD8, 0xFF]);
        let back = image::load_from_memory(&out).unwrap();
        assert_eq!((back.width(), back.height()), (64, 64));
        assert_eq!(COMPRESS_JPEG_QUALITY, 65);
    }

    #[test]
    fn job_ids_and_names_match_node() {
        assert_eq!(Kind::Compress.name(), "screenshot.compress");
        assert_eq!(Kind::Purge.job_id(42), "screenshot.purge:42");
        assert_eq!(Kind::parse("compress"), Some(Kind::Compress));
        assert_eq!(Kind::parse("purge"), Some(Kind::Purge));
        assert_eq!(Kind::parse("x"), None);
        assert_eq!(QUEUE_LIMITER_SPACING, Duration::from_secs(3));
        assert_eq!(SCREENSHOT_JOB_TIMEOUT, Duration::from_secs(120));
        assert!(LEASE > HANDLER_TIMEOUT && HANDLER_TIMEOUT > SCREENSHOT_JOB_TIMEOUT);
    }

    #[tokio::test]
    async fn invalid_payload_is_dropped_without_io() {
        let Ok(state) = AppState::for_test() else {
            return;
        };
        let limits = Limits::default();
        for data in [
            json!(null),
            json!({ "kind": "compress" }),
            json!({ "kind": "x", "screenshotId": 1 }),
            json!({ "kind": "purge", "screenshotId": "1" }),
            json!({ "kind": "purge", "screenshotId": 99_999_999_999_i64 }),
        ] {
            let job = Job {
                id: "x".into(),
                data,
                attempt: 1,
                max_attempts: 1,
                backoff_ms: 0,
                enqueued_at: 0,
            };
            assert!(matches!(
                handle_job(&state, &limits, job).await,
                Outcome::Drop(_)
            ));
        }
    }

    #[test]
    fn cron_registry() {
        let jobs = cron_jobs();
        let names: Vec<_> = jobs.iter().map(|j| j.name).collect();
        assert_eq!(
            names,
            [
                "screenshot-purge",
                "screenshot-compress",
                "screenshot-purge-deleted"
            ]
        );
    }
}
