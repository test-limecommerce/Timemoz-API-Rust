//! Port `admin-jobs.processor.ts` + `admin-jobs.handlers.ts` + boot reaper
//! (`AdminJobsService.reapStaleJobs`).
//!
//! Antrean: `mz_core::queue` bernama [`QUEUE`] (kunci Redis `mz:q:admin-jobs:*`, terpisah dari
//! Bull Node), concurrency 1 per proses seperti `@Process({ name: '*', concurrency: 1 })`.
//! Status tetap di tabel `adminJob`: QUEUED → RUNNING → DONE/FAILED lewat `UPDATE` bersyarat
//! status lama, jadi job yang diambil ulang (lease habis) atau baris yang sudah di-reap
//! dilewati. Selama RUNNING worker memperbarui `log` tiap 5 detik dan kunci detak
//! `mz:admin-jobs:alive:<uuid>` (TTL 30 detik) yang dipakai reaper sebagai pengganti
//! `job.getState() === 'active'` Bull.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fred::prelude::*;
use fred::types::Expiration;
use mz_core::AppState;
use mz_core::queue::{Job, Outcome, WorkerConfig};
use mz_operator::{AuditCtx, OperatorAuditEntry, record, strip_secrets};
use serde_json::{Map, Value, json};

use super::service::{
    BACKUP_PREFIX, BackupFile, JobType, LOG_FLUSH, LogTail, ORPHAN_QUEUED, QUEUE, RUNNER_MARK,
    append_log_tail, backup_bucket, backup_files_of, backup_location_of, job_id,
    legacy_database_url, lock_key,
};
use crate::company_data::{backup, errors::Log, merge, restore};

/// Lease antrean: lebih panjang dari timeout job terpanjang (MERGE 180 menit), supaya job yang
/// masih berjalan tidak diambil proses lain.
const LEASE: Duration = Duration::from_secs(200 * 60);
/// Batas handler di level antrean (timeout per tipe ditegakkan di dalam handler).
const HANDLER_TIMEOUT: Duration = Duration::from_secs(190 * 60);
/// TTL kunci detak; diperbarui tiap [`LOG_FLUSH`].
const ALIVE_TTL_SECS: i64 = 30;
/// Interval reaper periodik (Node hanya saat boot; lihat deviasi).
const REAP_EVERY: Duration = Duration::from_secs(10 * 60);

pub fn alive_key(uuid: &str) -> String {
    format!("mz:admin-jobs:alive:{uuid}")
}

fn is_uuid(s: &str) -> bool {
    mz_core::uuid_rules::is_uuid_shape(s)
}

/// Pasang worker antrean `admin-jobs` + reaper (saat start lalu tiap 10 menit).
pub fn spawn_worker(state: &AppState) -> tokio::task::JoinHandle<()> {
    let reaper_state = state.clone();
    // Loop jangka panjang: `tokio::spawn` (bukan `bg::spawn`, yang ditunggu saat shutdown).
    tokio::spawn(async move {
        loop {
            if let Err(e) = reap_stale_jobs(&reaper_state, mz_core::time::now_naive()).await {
                tracing::error!(error = %e, "Reaper admin job gagal");
            }
            tokio::time::sleep(REAP_EVERY).await;
        }
    });
    let cfg = WorkerConfig {
        concurrency: 1,
        lease: LEASE,
        handler_timeout: HANDLER_TIMEOUT,
        poll: mz_core::queue::DEFAULT_POLL,
    };
    let st = state.clone();
    mz_core::queue::spawn_worker_with(state, QUEUE, cfg, move |job: Job| {
        let uuid = job
            .data
            .get("uuid")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let fut = uuid.map(|u| process_owned(st.clone(), u));
        async move {
            if let Some(fut) = fut {
                fut.await;
            }
            Outcome::Done
        }
    })
}

/// `process` dengan argumen milik sendiri dan future `Send` eksplisit (menghindari batasan
/// inferensi `Send` rustc untuk future ber-referensi di dalam closure generik).
fn process_owned(
    state: AppState,
    uuid: String,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move { process(&state, &uuid).await })
}

fn lock_tail(t: &Mutex<LogTail>) -> std::sync::MutexGuard<'_, LogTail> {
    t.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Baris `adminJob` yang dibaca processor.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct JobRow {
    pub id: i32,
    pub uuid: uuid::Uuid,
    #[sqlx(rename = "type")]
    pub ty: String,
    pub status: String,
    #[sqlx(rename = "companyId")]
    pub company_id: Option<i32>,
    pub params: Value,
    #[sqlx(rename = "dryRun")]
    pub dry_run: bool,
    #[sqlx(rename = "operatorId")]
    pub operator_id: i32,
}

/// `summarize(result)`: tanpa array (mis. daftar file backup).
fn summarize(result: &Map<String, Value>) -> Value {
    Value::Object(
        result
            .iter()
            .filter(|(_, v)| !v.is_array())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )
}

/// `AdminJobsProcessor.handle(job)`.
pub async fn process(state: &AppState, uuid: &str) {
    if !is_uuid(uuid) {
        return;
    }
    let row: Option<JobRow> = match sqlx::query_as(
        r#"SELECT id, uuid, type::text AS type, status::text AS status, "companyId", params,
                  "dryRun", "operatorId"
             FROM "adminJob" WHERE uuid = $1::uuid"#,
    )
    .bind(uuid)
    .fetch_optional(&state.db)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(job = uuid, error = %e, "adminJob tidak terbaca");
            return;
        }
    };
    let Some(row) = row else {
        tracing::warn!(job = uuid, "adminJob tidak ditemukan — dilewati");
        return;
    };
    if row.status != "QUEUED" {
        tracing::warn!(job = uuid, status = %row.status, "adminJob bukan QUEUED — dilewati");
        return;
    }
    let claimed = sqlx::query(
        r#"UPDATE "adminJob" SET status = 'RUNNING', "startedAt" = $2 WHERE id = $1 AND status = 'QUEUED'"#,
    )
    .bind(row.id)
    .bind(mz_core::time::now_naive())
    .execute(&state.db)
    .await;
    if !matches!(claimed, Ok(r) if r.rows_affected() == 1) {
        return;
    }

    let tail = Arc::new(Mutex::new(LogTail::default()));
    lock_tail(&tail).append(&format!(
        "Start {}{}",
        row.ty,
        if row.dry_run { " (dry run)" } else { "" }
    ));
    set_alive(state, uuid).await;

    // Flush log + detak tiap 5 detik.
    let flusher = {
        let state = state.clone();
        let tail = Arc::clone(&tail);
        let id = row.id;
        let uuid = uuid.to_owned();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(LOG_FLUSH);
            tick.tick().await;
            loop {
                tick.tick().await;
                set_alive(&state, &uuid).await;
                let value = {
                    let mut t = tail
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if !t.take_dirty() {
                        continue;
                    }
                    t.value()
                };
                if let Err(e) = sqlx::query(
                    r#"UPDATE "adminJob" SET log = $2 WHERE id = $1 AND status = 'RUNNING'"#,
                )
                .bind(id)
                .bind(value)
                .execute(&state.db)
                .await
                {
                    tracing::warn!(job = %uuid, error = %e, "Flush log adminJob gagal");
                }
            }
        })
    };

    let log = TailLog(Arc::clone(&tail));
    let outcome = match JobType::parse(&row.ty) {
        None => Err(anyhow::anyhow!("No handler registered for {}", row.ty)),
        Some(ty) => {
            match tokio::time::timeout(ty.timeout(), run_handler(state, ty, &row, &log)).await {
                Ok(r) => r,
                Err(_) => Err(anyhow::anyhow!(
                    "Job timed out after {} minutes",
                    ty.timeout().as_secs() / 60
                )),
            }
        }
    };
    flusher.abort();
    let _ = flusher.await;

    let (status, result, failure) = match outcome {
        Ok(result) => {
            lock_tail(&tail).append(&format!("Done {}", row.ty));
            ("DONE", Some(result), None)
        }
        Err(err) => {
            let msg = err.to_string();
            lock_tail(&tail).append(&format!("FAILED: {msg}"));
            tracing::error!(job = uuid, ty = %row.ty, error = %msg, "adminJob gagal");
            ("FAILED", None, Some(msg))
        }
    };
    let stored = match (&result, &failure) {
        (Some(r), _) => strip_secrets(&Value::Object(r.clone()), 0),
        (None, f) => json!({ "error": f }),
    };
    let log_value = lock_tail(&tail).value();
    if let Err(e) = finish(
        state,
        &row,
        status,
        &stored,
        &log_value,
        result.as_ref(),
        failure.as_deref(),
    )
    .await
    {
        // Baris tetap RUNNING; reaper berikutnya yang menutupnya.
        tracing::error!(job = uuid, error = %e, "Gagal menutup adminJob");
    }
    clear_alive(state, uuid).await;
}

async fn finish(
    state: &AppState,
    row: &JobRow,
    status: &str,
    stored: &Value,
    log: &str,
    result: Option<&Map<String, Value>>,
    failure: Option<&str>,
) -> sqlx::Result<()> {
    let mut tx = state.db.begin().await?;
    let res = sqlx::query(
        r#"UPDATE "adminJob" SET status = $2::"AdminJobStatus", result = $3, log = $4, "finishedAt" = $5
            WHERE id = $1 AND status = 'RUNNING'"#,
    )
    .bind(row.id)
    .bind(status)
    .bind(stored)
    .bind(log)
    .bind(mz_core::time::now_naive())
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() != 1 {
        tracing::warn!(job = %row.uuid, "adminJob sudah tidak RUNNING saat selesai — status tidak ditimpa");
        tx.commit().await?;
        return Ok(());
    }
    let mut after = Map::new();
    after.insert("type".into(), json!(row.ty));
    after.insert("status".into(), json!(status));
    after.insert("dryRun".into(), json!(row.dry_run));
    match result {
        Some(r) if status == "DONE" => {
            after.insert("result".into(), summarize(r));
        }
        _ => {
            after.insert("error".into(), json!(failure));
        }
    }
    record(
        &mut *tx,
        &AuditCtx::default(),
        &OperatorAuditEntry {
            operator_id: row.operator_id,
            action: "admin_job.finish",
            target_type: "adminJob",
            target_uuid: Some(row.uuid),
            company_id: row.company_id,
            before: None,
            after: Some(&Value::Object(after)),
            reason: None,
        },
    )
    .await?;
    tx.commit().await
}

async fn set_alive(state: &AppState, uuid: &str) {
    let res: Result<Option<String>, _> = state
        .redis
        .next()
        .set(
            alive_key(uuid),
            "1",
            Some(Expiration::EX(ALIVE_TTL_SECS)),
            None,
            false,
        )
        .await;
    if let Err(e) = res {
        tracing::warn!(job = uuid, error = %e, "Detak adminJob gagal ditulis");
    }
}

async fn clear_alive(state: &AppState, uuid: &str) {
    let res: Result<i64, _> = state.redis.next().del(alive_key(uuid)).await;
    if let Err(e) = res {
        tracing::warn!(job = uuid, error = %e, "Detak adminJob gagal dihapus");
    }
}

// ── Handler ─────────────────────────────────────────────────────────────────

/// Penerima baris log untuk handler yang didaftarkan crate lain.
pub type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

/// Handler `APPLY_HOLIDAYS` (`NationalHolidaysService.onModuleInit` di Node). Didaftarkan dari luar
/// (`mz-leave-holidays` bergantung pada crate ini, jadi arah sebaliknya akan siklus).
pub type ApplyHolidaysFn = fn(
    AppState,
    Map<String, Value>,
    LogFn,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = anyhow::Result<Map<String, Value>>> + Send>,
>;

static APPLY_HOLIDAYS: std::sync::OnceLock<ApplyHolidaysFn> = std::sync::OnceLock::new();

/// Daftarkan handler `APPLY_HOLIDAYS`; panggil sekali sebelum [`spawn_worker`]. Panggilan kedua diabaikan.
pub fn register_apply_holidays(f: ApplyHolidaysFn) {
    let _ = APPLY_HOLIDAYS.set(f);
}

/// Penerima log untuk inti company-data: baris masuk ke ekor log job.
pub struct TailLog(pub Arc<Mutex<LogTail>>);

impl Log for TailLog {
    fn line(&self, line: &str) {
        lock_tail(&self.0).append(line);
    }
}

async fn run_handler(
    state: &AppState,
    ty: JobType,
    row: &JobRow,
    log: &TailLog,
) -> anyhow::Result<Map<String, Value>> {
    match ty {
        JobType::Backup => handle_backup(state, row, log).await,
        JobType::Restore => handle_restore(state, row, log).await,
        JobType::Merge => handle_merge(state, row, log).await,
        JobType::ApplyHolidays => match APPLY_HOLIDAYS.get() {
            Some(f) => {
                let tail = Arc::clone(&log.0);
                let params = row.params.as_object().cloned().unwrap_or_default();
                f(
                    state.clone(),
                    params,
                    Arc::new(move |l: &str| lock_tail(&tail).append(l)),
                )
                .await
            }
            None => anyhow::bail!("No handler registered for APPLY_HOLIDAYS"),
        },
    }
}

/// Satu segmen nama file backup: tanpa `..`, tanpa awalan titik, tanpa karakter path lain.
fn safe_segment(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `safeJoin(root, name)`: path yang dijamin tetap di dalam `root`.
pub fn safe_join(root: &Path, name: &str) -> anyhow::Result<PathBuf> {
    if !name.split('/').all(safe_segment) {
        anyhow::bail!("Unsafe backup file name: {name}");
    }
    Ok(root.join(name))
}

/// `listFilesRecursive(dir)`: path relatif ber-`/`, terurut.
pub fn list_files_recursive(dir: &Path) -> std::io::Result<Vec<String>> {
    fn walk(root: &Path, rel: &str, out: &mut Vec<String>) -> std::io::Result<()> {
        let path = if rel.is_empty() {
            root.to_path_buf()
        } else {
            root.join(rel)
        };
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let ft = entry.file_type()?;
            if ft.is_dir() {
                walk(root, &child, out)?;
            } else if ft.is_file() {
                out.push(child);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, "", &mut out)?;
    out.sort();
    Ok(out)
}

fn content_type_of(name: &str) -> &'static str {
    if name.ends_with(".gz") {
        "application/gzip"
    } else if name.ends_with(".json") {
        "application/json"
    } else if name.ends_with(".ndjson") {
        "application/x-ndjson"
    } else {
        "application/octet-stream"
    }
}

/// `manifestSummary(manifest)`.
fn manifest_summary(m: &Value) -> Value {
    let Some(m) = m.as_object() else {
        return json!({});
    };
    let get = |k: &str| m.get(k).cloned().unwrap_or(Value::Null);
    let source = m
        .get("source")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let sget = |k: &str| source.get(k).cloned().unwrap_or(Value::Null);
    json!({
        "formatVersion": get("formatVersion"),
        "generatedAt": get("generatedAt"),
        "finishedAt": get("finishedAt"),
        "tableCount": m.get("order").and_then(Value::as_array).map_or(Value::Null, |a| json!(a.len())),
        "totals": get("totals"),
        "options": get("options"),
        "source": {
            "companyId": sget("companyId"),
            "companyUuid": sget("companyUuid"),
            "companyName": sget("companyName"),
        },
    })
}

/// Temp dir yang SELALU dihapus (`finally { rm(dir) }`).
struct TempDir(PathBuf);

impl TempDir {
    fn create(label: &str) -> std::io::Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "admin-job-{label}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `AdminJobsHandlers.backup`: backup gzip ke temp dir lalu upload ke
/// `gs://$ADMIN_BACKUP_BUCKET/company-backups/<jobUuid>/…`.
async fn handle_backup(
    state: &AppState,
    row: &JobRow,
    log: &dyn Log,
) -> anyhow::Result<Map<String, Value>> {
    let Some(bucket) = backup_bucket(state) else {
        anyhow::bail!("ADMIN_BACKUP_BUCKET_NOT_CONFIGURED");
    };
    let Some(company_id) = row.company_id else {
        anyhow::bail!("Backup job has no company");
    };
    let dir = TempDir::create("backup")?;
    let mut conn = state.db.acquire().await?;
    let manifest = backup::backup_company(
        &mut conn,
        &backup::BackupParams {
            company_id,
            out_dir: dir.0.clone(),
            gzip: true,
            skip_heavy: row.params.get("skipHeavy") == Some(&Value::Bool(true)),
            redact_secrets: row.params.get("redactSecrets") == Some(&Value::Bool(true)),
            database_url: Some(state.config.database_url.clone()),
        },
        log,
    )
    .await?;
    drop(conn);

    let prefix = format!("{BACKUP_PREFIX}/{}/", row.uuid);
    let mut uploaded = Vec::new();
    for name in list_files_recursive(&dir.0)? {
        let local = safe_join(&dir.0, &name)?;
        let size = std::fs::metadata(&local)?.len();
        mz_files::upload_local_file(
            state,
            &local,
            &format!("{prefix}{name}"),
            &bucket,
            Some(content_type_of(&name)),
        )
        .await?;
        uploaded.push(json!({ "name": name, "size": size }));
    }
    log.line(&format!(
        "Uploaded {} file(s) to gs://{bucket}/{prefix}",
        uploaded.len()
    ));
    let mut out = Map::new();
    out.insert("bucket".into(), json!(bucket));
    out.insert("prefix".into(), json!(prefix));
    out.insert("files".into(), Value::Array(uploaded));
    out.insert("manifest".into(), manifest_summary(&manifest));
    Ok(out)
}

/// `AdminJobsHandlers.restore`: unduh prefix backup ke temp dir lalu restore `clone`.
async fn handle_restore(
    state: &AppState,
    row: &JobRow,
    log: &dyn Log,
) -> anyhow::Result<Map<String, Value>> {
    let Some(env_bucket) = backup_bucket(state) else {
        anyhow::bail!("ADMIN_BACKUP_BUCKET_NOT_CONFIGURED");
    };
    if row.params.get("mode").and_then(Value::as_str) != Some("clone") {
        anyhow::bail!("Only mode \"clone\" is supported");
    }
    let backup_job_uuid = match row.params.get("backupJobUuid") {
        Some(Value::String(s)) => s.clone(),
        None | Some(Value::Null) => String::new(),
        Some(other) => other.to_string(),
    };
    let backup: Option<(String, String, Option<Value>)> = if is_uuid(&backup_job_uuid) {
        sqlx::query_as(
            r#"SELECT type::text, status::text, result FROM "adminJob" WHERE uuid = $1::uuid"#,
        )
        .bind(&backup_job_uuid)
        .fetch_optional(&state.db)
        .await?
    } else {
        None
    };
    let Some((_, _, result)) = backup.filter(|(t, s, _)| t == "BACKUP_COMPANY" && s == "DONE")
    else {
        anyhow::bail!("Backup job {backup_job_uuid} is not a finished backup");
    };
    let (prefix, bucket) = backup_location_of(result.as_ref());
    let files: Vec<BackupFile> = backup_files_of(result.as_ref());
    let Some(prefix) = prefix.filter(|_| !files.is_empty()) else {
        anyhow::bail!("Backup job {backup_job_uuid} has no stored files");
    };
    let bucket = bucket.unwrap_or(env_bucket);

    let dir = TempDir::create("restore")?;
    for f in &files {
        let dest = safe_join(&dir.0, &f.name)?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        mz_files::download_to_file(state, &format!("{prefix}{}", f.name), &dest, &bucket).await?;
    }
    log.line(&format!(
        "Downloaded {} file(s) from gs://{bucket}/{prefix}",
        files.len()
    ));
    let users = if row.params.get("users").and_then(Value::as_str) == Some("insert") {
        restore::UsersMode::Insert
    } else {
        restore::UsersMode::Reuse
    };
    let result = restore::restore_company(
        &state.db,
        &restore::RestoreParams {
            dir: dir.0.clone(),
            dry_run: row.dry_run,
            users,
            username_suffix: if users == restore::UsersMode::Insert {
                row.params
                    .get("usernameSuffix")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            } else {
                None
            },
            rename: row
                .params
                .get("rename")
                .and_then(Value::as_str)
                .map(str::to_owned),
            database_url: Some(state.config.database_url.clone()),
            lock_key: Some(lock_key(row.company_id)),
        },
        log,
    )
    .await?;
    let mut out = Map::new();
    out.insert("backupJobUuid".into(), json!(backup_job_uuid));
    if let Value::Object(m) = result {
        out.extend(m);
    }
    Ok(out)
}

fn param_str(params: &Value, k: &str) -> Option<String> {
    params.get(k).and_then(Value::as_str).map(str::to_owned)
}

/// `AdminJobsHandlers.merge`: merge dari DB lama (`LEGACY_DATABASE_URL`, mode `--from-db`).
async fn handle_merge(
    state: &AppState,
    row: &JobRow,
    log: &dyn Log,
) -> anyhow::Result<Map<String, Value>> {
    let Some(legacy_url) = legacy_database_url(state) else {
        anyhow::bail!("LEGACY_DATABASE_NOT_CONFIGURED");
    };
    let Some(company_id) = row.company_id else {
        anyhow::bail!("Merge job has no company");
    };
    let legacy_options = merge::pg_connect_options(
        "LEGACY_DATABASE_URL",
        Some(&legacy_url),
        state.config.var("PGSSLROOTCERT").as_deref(),
    )?;
    let legacy_company_id = row
        .params
        .get("legacyCompanyId")
        .and_then(Value::as_f64)
        .unwrap_or(f64::NAN);
    // `Number(job.params.legacyCompanyId)`; validasi merge menolak yang bukan bilangan bulat > 0.
    #[allow(clippy::cast_possible_truncation)]
    let legacy_company_id = if legacy_company_id.fract() == 0.0
        && legacy_company_id >= 1.0
        && legacy_company_id <= f64::from(i32::MAX)
    {
        legacy_company_id as i32
    } else {
        0
    };
    let result = merge::merge_legacy_company(
        &state.db,
        &legacy_options,
        &merge::MergeParams {
            legacy_company_id,
            apply: !row.dry_run,
            prefer_legacy: row.params.get("preferLegacy") == Some(&Value::Bool(true)),
            unlimited_retention: param_str(&row.params, "retention").as_deref()
                == Some("UNLIMITED"),
            accept_retention: param_str(&row.params, "retention").as_deref() == Some("ACCEPT"),
            since: param_str(&row.params, "since"),
            until: param_str(&row.params, "until"),
            expect_target_company_id: Some(company_id),
            target_host: merge::url_host(&state.config.database_url),
            legacy_host: merge::url_host(&legacy_url),
            lock_key: Some(lock_key(row.company_id)),
        },
        log,
    )
    .await?;
    Ok(match result {
        Value::Object(m) => m,
        _ => Map::new(),
    })
}

// ── Reaper ──────────────────────────────────────────────────────────────────

#[derive(Debug, sqlx::FromRow)]
struct ActiveRow {
    id: i32,
    uuid: uuid::Uuid,
    #[sqlx(rename = "type")]
    ty: String,
    status: String,
    #[sqlx(rename = "companyId")]
    company_id: Option<i32>,
    #[sqlx(rename = "operatorId")]
    operator_id: i32,
    #[sqlx(rename = "startedAt")]
    started_at: Option<chrono::NaiveDateTime>,
    #[sqlx(rename = "createdAt")]
    created_at: chrono::NaiveDateTime,
    log: Option<String>,
}

/// Keadaan job di antrean Rust (`job.getState()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveState {
    /// Detak worker masih ada (`active`).
    Running,
    /// Masih di antrean (`waiting`/`delayed`).
    Queued,
    /// Tidak ada jejak hidup.
    Gone,
}

/// `staleReason(row, now)` — murni, untuk diuji.
pub fn stale_reason(
    status: &str,
    ty: JobType,
    started_at: Option<chrono::NaiveDateTime>,
    created_at: chrono::NaiveDateTime,
    now: chrono::NaiveDateTime,
    live: LiveState,
) -> Option<&'static str> {
    if status == "RUNNING" {
        let since = started_at.unwrap_or(created_at);
        let age = now - since;
        if age > chrono::Duration::from_std(ty.timeout()).unwrap_or(chrono::Duration::MAX) {
            return Some("worker restarted (job exceeded its timeout)");
        }
        return if live == LiveState::Running {
            None
        } else {
            Some("worker restarted (no live queue job)")
        };
    }
    if now - created_at < ORPHAN_QUEUED {
        return None;
    }
    if live == LiveState::Gone {
        Some("queue job lost before it started")
    } else {
        None
    }
}

async fn live_state(state: &AppState, uuid: &str) -> LiveState {
    let alive: Result<i64, _> = state.redis.next().exists(alive_key(uuid)).await;
    if matches!(alive, Ok(n) if n > 0) {
        return LiveState::Running;
    }
    match mz_core::queue::get(state, QUEUE, &job_id(uuid)).await {
        Ok(Some(_)) => LiveState::Queued,
        // Redis bermasalah: anggap hidup (jangan menandai FAILED karena tidak tahu).
        Err(_) => LiveState::Running,
        Ok(None) => LiveState::Gone,
    }
}

/// `reapStaleJobs(now)` untuk job MILIK Rust (penanda `runner` di audit `admin_job.create`):
/// job yang tak akan pernah selesai → FAILED + satu baris log + audit `admin_job.finish`.
pub async fn reap_stale_jobs(state: &AppState, now: chrono::NaiveDateTime) -> anyhow::Result<u64> {
    let rows: Vec<ActiveRow> = sqlx::query_as(
        r#"SELECT j.id, j.uuid, j.type::text AS type, j.status::text AS status, j."companyId",
                  j."operatorId", j."startedAt", j."createdAt", j.log
             FROM "adminJob" j
            WHERE j.status IN ('QUEUED', 'RUNNING')
              AND EXISTS (SELECT 1 FROM "operatorAuditLog" a
                           WHERE a.action = 'admin_job.create' AND a."targetUuid" = j.uuid
                             AND a.after->>'runner' = $1)"#,
    )
    .bind(RUNNER_MARK)
    .fetch_all(&state.db)
    .await?;
    let mut reaped = 0;
    for row in rows {
        let Some(ty) = JobType::parse(&row.ty) else {
            continue;
        };
        let live = live_state(state, &row.uuid.to_string()).await;
        let Some(reason) = stale_reason(&row.status, ty, row.started_at, row.created_at, now, live)
        else {
            continue;
        };
        let mut tx = state.db.begin().await?;
        let res = sqlx::query(
            r#"UPDATE "adminJob" SET status = 'FAILED', "finishedAt" = $3, log = $4
                WHERE id = $1 AND status = $2::"AdminJobStatus""#,
        )
        .bind(row.id)
        .bind(&row.status)
        .bind(now)
        .bind(append_log_tail(
            row.log.as_deref(),
            &format!("Marked FAILED by boot reaper: {reason}"),
        ))
        .execute(&mut *tx)
        .await?;
        if res.rows_affected() != 1 {
            continue;
        }
        record(
            &mut *tx,
            &AuditCtx::default(),
            &OperatorAuditEntry {
                operator_id: row.operator_id,
                action: "admin_job.finish",
                target_type: "adminJob",
                target_uuid: Some(row.uuid),
                company_id: row.company_id,
                before: Some(&json!({ "status": row.status })),
                after: Some(&json!({ "type": row.ty, "status": "FAILED", "error": reason })),
                reason: None,
            },
        )
        .await?;
        tx.commit().await?;
        reaped += 1;
        tracing::warn!(job = %row.uuid, ty = %row.ty, reason, "Admin job ditandai FAILED");
    }
    Ok(reaped)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use chrono::NaiveDateTime;

    fn t(s: &str) -> NaiveDateTime {
        mz_core::time::parse_js_date(s).unwrap()
    }

    #[test]
    fn reaper_seperti_node() {
        let now = t("2026-11-12T10:00:00.000Z");
        let min = chrono::Duration::minutes;
        // RUNNING melewati timeout tipenya → FAILED walau masih hidup.
        assert_eq!(
            stale_reason(
                "RUNNING",
                JobType::Backup,
                Some(now - min(61)),
                now - min(62),
                now,
                LiveState::Running
            ),
            Some("worker restarted (job exceeded its timeout)")
        );
        // RUNNING tanpa detak → FAILED.
        assert_eq!(
            stale_reason(
                "RUNNING",
                JobType::Backup,
                Some(now - min(5)),
                now - min(6),
                now,
                LiveState::Gone
            ),
            Some("worker restarted (no live queue job)")
        );
        // RUNNING hidup (MERGE 90 menit) → dibiarkan.
        assert_eq!(
            stale_reason(
                "RUNNING",
                JobType::Merge,
                Some(now - min(90)),
                now - min(91),
                now,
                LiveState::Running
            ),
            None
        );
        // QUEUED muda dibiarkan; tua tanpa jejak → FAILED; tua masih di antrean → dibiarkan.
        assert_eq!(
            stale_reason(
                "QUEUED",
                JobType::Backup,
                None,
                now - min(1),
                now,
                LiveState::Gone
            ),
            None
        );
        assert_eq!(
            stale_reason(
                "QUEUED",
                JobType::Backup,
                None,
                now - min(30),
                now,
                LiveState::Gone
            ),
            Some("queue job lost before it started")
        );
        assert_eq!(
            stale_reason(
                "QUEUED",
                JobType::Backup,
                None,
                now - min(30),
                now,
                LiveState::Queued
            ),
            None
        );
    }

    #[test]
    fn safe_join_menolak_nama_berbahaya() {
        let root = Path::new("/tmp/x");
        assert!(safe_join(root, "data/users.ndjson.gz").is_ok());
        assert!(safe_join(root, "manifest.json").is_ok());
        for bad in [
            "../etc/passwd",
            "data/../../x",
            ".hidden",
            "a//b",
            "/abs",
            "a\\b",
            "",
        ] {
            assert!(safe_join(root, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ringkasan_dan_content_type() {
        assert_eq!(content_type_of("data/a.ndjson.gz"), "application/gzip");
        assert_eq!(content_type_of("manifest.json"), "application/json");
        assert_eq!(content_type_of("a.ndjson"), "application/x-ndjson");
        assert_eq!(content_type_of("a.bin"), "application/octet-stream");
        let s = manifest_summary(&json!({
            "formatVersion": 2, "order": ["company", "users"], "totals": { "rows": 2 },
            "source": { "companyId": 5, "companyUuid": "u", "companyName": "PT", "database": "h/db" }
        }));
        assert_eq!(s["tableCount"], 2);
        assert_eq!(
            s["source"],
            json!({ "companyId": 5, "companyUuid": "u", "companyName": "PT" })
        );
        assert!(s["generatedAt"].is_null());
        assert_eq!(
            summarize(json!({ "a": 1, "files": [1] }).as_object().unwrap()),
            json!({ "a": 1 })
        );
    }

    #[test]
    fn list_files_terurut() {
        let dir = TempDir::create("spec").unwrap();
        std::fs::create_dir_all(dir.0.join("data")).unwrap();
        std::fs::write(dir.0.join("manifest.json"), "{}").unwrap();
        std::fs::write(dir.0.join("data/users.ndjson.gz"), "x").unwrap();
        std::fs::write(dir.0.join("data/company.ndjson.gz"), "x").unwrap();
        assert_eq!(
            list_files_recursive(&dir.0).unwrap(),
            vec![
                "data/company.ndjson.gz",
                "data/users.ndjson.gz",
                "manifest.json"
            ]
        );
        let path = dir.0.clone();
        drop(dir);
        assert!(!path.exists());
    }
}
