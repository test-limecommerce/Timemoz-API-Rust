//! Port `src/admin-jobs/admin-jobs.service.ts` + `admin-jobs.constants.ts`.
//!
//! Status job tinggal di tabel `adminJob` (bukan di antrean). Klaim satu job aktif per company
//! dijaga advisory lock transaksi `admin-job:<companyId|global>` + cek QUEUED/RUNNING, dengan
//! partial unique index `adminJob_one_active_per_company` sebagai penahan terakhir — kunci dan
//! index yang SAMA dengan Node, jadi job Rust dan Node tidak pernah aktif bersamaan untuk satu
//! company.

use std::time::Duration;

use axum::http::StatusCode;
use chrono::NaiveDateTime;
use mz_core::time::JsDate;
use mz_core::{ApiError, AppState};
use mz_operator::{AuditCtx, OperatorAuditEntry, record, strip_secrets};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// Nama antrean `mz_core::queue` (kunci Redis `mz:q:admin-jobs:*`, terpisah dari Bull).
pub const QUEUE: &str = "admin-jobs";
const MINUTE: u64 = 60;

/// `AdminJobType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobType {
    Backup,
    Restore,
    Merge,
    ApplyHolidays,
}

impl JobType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Backup => "BACKUP_COMPANY",
            Self::Restore => "RESTORE_COMPANY",
            Self::Merge => "MERGE_COMPANY",
            Self::ApplyHolidays => "APPLY_HOLIDAYS",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "BACKUP_COMPANY" => Self::Backup,
            "RESTORE_COMPANY" => Self::Restore,
            "MERGE_COMPANY" => Self::Merge,
            "APPLY_HOLIDAYS" => Self::ApplyHolidays,
            _ => return None,
        })
    }

    /// `ADMIN_JOB_TIMEOUT_MS`.
    pub fn timeout(self) -> Duration {
        Duration::from_secs(
            MINUTE
                * match self {
                    Self::Backup | Self::Restore => 60,
                    Self::Merge => 180,
                    Self::ApplyHolidays => 20,
                },
        )
    }

    /// `DRY_RUN_REQUIRED_TYPES`.
    pub fn dry_run_required(self) -> bool {
        matches!(self, Self::Restore | Self::Merge)
    }

    /// `BUCKET_REQUIRED_TYPES`.
    pub fn bucket_required(self) -> bool {
        matches!(self, Self::Backup | Self::Restore)
    }
}

pub const JOB_TYPES: &[&str] = &[
    "BACKUP_COMPANY",
    "RESTORE_COMPANY",
    "MERGE_COMPANY",
    "APPLY_HOLIDAYS",
];
pub const JOB_STATUSES: &[&str] = &["QUEUED", "RUNNING", "DONE", "FAILED"];

/// `DRY_RUN_VALIDITY_MS`.
pub const DRY_RUN_VALIDITY: chrono::Duration = chrono::Duration::hours(24);
/// `ADMIN_JOB_LOG_CAP`.
pub const LOG_CAP: usize = 200_000;
/// `ADMIN_JOB_LOG_FLUSH_MS`.
pub const LOG_FLUSH: Duration = Duration::from_secs(5);
/// `ADMIN_JOB_ORPHAN_QUEUED_MS`.
pub const ORPHAN_QUEUED: chrono::Duration = chrono::Duration::minutes(10);
/// `ADMIN_BACKUP_URL_TTL_SECONDS`.
pub const BACKUP_URL_TTL_SECONDS: i64 = 900;
/// `ADMIN_BACKUP_PREFIX`.
pub const BACKUP_PREFIX: &str = "company-backups";
/// `ADMIN_JOB_CLAIM_TX_TIMEOUT_MS`: di sini batas tunggu advisory lock klaim.
pub const CLAIM_LOCK_TIMEOUT: &str = "10s";

/// Penanda pemilik job di `after` audit `admin_job.create`: reaper Rust hanya menyentuh job
/// yang dibuat (dan dijalankan) Rust.
pub const RUNNER_MARK: &str = "rust";

/// `adminJobBullId(uuid)` (id job di antrean Rust).
pub fn job_id(uuid: &str) -> String {
    format!("admin-job:{uuid}")
}

/// `adminJobLockKey(companyId)`.
pub fn lock_key(company_id: Option<i32>) -> String {
    match company_id {
        Some(id) => format!("admin-job:{id}"),
        None => "admin-job:global".to_owned(),
    }
}

/// `adminBackupBucket()`.
pub fn backup_bucket(state: &AppState) -> Option<String> {
    state
        .config
        .var("ADMIN_BACKUP_BUCKET")
        .map(|v| mz_core::jsfmt::trim(&v).to_owned())
        .filter(|v| !v.is_empty())
}

/// `process.env.LEGACY_DATABASE_URL?.trim()`.
pub fn legacy_database_url(state: &AppState) -> Option<String> {
    state
        .config
        .var("LEGACY_DATABASE_URL")
        .map(|v| mz_core::jsfmt::trim(&v).to_owned())
        .filter(|v| !v.is_empty())
}

// ── Hash & log ──────────────────────────────────────────────────────────────

fn sort_keys(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .filter_map(|k| m.get(k).map(|c| (k.clone(), sort_keys(c))))
                    .collect(),
            )
        }
        Value::Array(a) => Value::Array(a.iter().map(sort_keys).collect()),
        other => other.clone(),
    }
}

/// `canonicalJson(value)`: kunci objek diurutkan rekursif (`undefined` tidak ada di JSON).
pub fn canonical_json(v: &Value) -> String {
    sort_keys(v).to_string()
}

/// `adminJobParamsHash(params)`: `sha256(canonicalJson(params tanpa dryRun))`.
pub fn params_hash(params: &Map<String, Value>) -> String {
    let mut rest = params.clone();
    rest.shift_remove("dryRun");
    hex::encode(Sha256::digest(
        canonical_json(&Value::Object(rest)).as_bytes(),
    ))
}

/// Ekor `cap` karakter terakhir (unit UTF-16 seperti `String.slice` JS, dibulatkan ke batas char).
pub fn tail(text: &str, cap: usize) -> String {
    let units = text.encode_utf16().count();
    if units <= cap {
        return text.to_owned();
    }
    let mut skip = units - cap;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if skip == 0 {
            start = i;
            break;
        }
        skip = skip.saturating_sub(c.len_utf16());
        start = i + c.len_utf8();
    }
    text.get(start..).unwrap_or_default().to_owned()
}

/// `appendLogTail(log, line)`.
pub fn append_log_tail(log: Option<&str>, line: &str) -> String {
    let next = format!(
        "{}{} {line}\n",
        log.unwrap_or_default(),
        JsDate::now().to_js_string()
    );
    tail(&next, LOG_CAP)
}

/// `LogTail` processor: baris ditambah murah, `cap` karakter terakhir disimpan.
#[derive(Debug, Default)]
pub struct LogTail {
    text: String,
    dirty: bool,
}

impl LogTail {
    pub fn append(&mut self, line: &str) {
        self.text.push_str(&JsDate::now().to_js_string());
        self.text.push(' ');
        self.text.push_str(line);
        self.text.push('\n');
        if self.text.len() > LOG_CAP * 2 {
            self.text = tail(&self.text, LOG_CAP);
        }
        self.dirty = true;
    }

    pub fn value(&self) -> String {
        tail(&self.text, LOG_CAP)
    }

    /// `true` bila ada baris baru sejak panggilan terakhir.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }
}

// ── Result backup ───────────────────────────────────────────────────────────

/// `BackupFileEntry`.
#[derive(Debug, Clone, PartialEq)]
pub struct BackupFile {
    pub name: String,
    pub size: f64,
}

/// `backupFilesOf(result)`.
pub fn backup_files_of(result: Option<&Value>) -> Vec<BackupFile> {
    let Some(Value::Array(files)) = result
        .and_then(|r| r.as_object())
        .and_then(|r| r.get("files"))
    else {
        return vec![];
    };
    files
        .iter()
        .filter_map(|f| {
            let o = f.as_object()?;
            let name = o.get("name")?.as_str()?.to_owned();
            let size = match o.get("size") {
                None | Some(Value::Null) => 0.0,
                Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
                Some(Value::String(s)) => mz_core::jsfmt::number(s),
                Some(Value::Bool(b)) => f64::from(u8::from(*b)),
                Some(_) => f64::NAN,
            };
            Some(BackupFile { name, size })
        })
        .collect()
}

/// `backupLocationOf(result)`: `(prefix, bucket)`.
pub fn backup_location_of(result: Option<&Value>) -> (Option<String>, Option<String>) {
    let Some(o) = result.and_then(Value::as_object) else {
        return (None, None);
    };
    let s = |k: &str| o.get(k).and_then(Value::as_str).map(str::to_owned);
    (s("prefix"), s("bucket"))
}

// ── Error ───────────────────────────────────────────────────────────────────

pub fn job_already_active() -> ApiError {
    ApiError::coded(
        StatusCode::CONFLICT,
        "JOB_ALREADY_ACTIVE",
        "Another admin job for this company is still queued or running",
    )
}

pub fn bucket_not_configured() -> ApiError {
    ApiError::coded(
        StatusCode::PRECONDITION_FAILED,
        "ADMIN_BACKUP_BUCKET_NOT_CONFIGURED",
        "ADMIN_BACKUP_BUCKET is not configured on the server",
    )
}

fn job_not_found() -> ApiError {
    ApiError::coded(
        StatusCode::NOT_FOUND,
        "ADMIN_JOB_NOT_FOUND",
        "Admin job not found",
    )
}

fn not_downloadable(message: &str) -> ApiError {
    ApiError::coded(StatusCode::CONFLICT, "JOB_NOT_DOWNLOADABLE", message)
}

fn is_lock_timeout(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(db) if db.code().as_deref() == Some("55P03"))
}

// ── createJob ───────────────────────────────────────────────────────────────

/// `CreateAdminJobInput`.
#[derive(Debug, Clone)]
pub struct CreateJob {
    pub ty: JobType,
    pub company_id: Option<i32>,
    pub params: Map<String, Value>,
    pub dry_run: bool,
    pub operator_id: i32,
    pub reason: Option<String>,
}

/// `createJob(input)`: cek bucket (412) → transaksi (lock, cek aktif 409, cek dry run 409,
/// insert, audit `admin_job.create`) → enqueue sesudah commit. Gagal antre → FAILED + 503.
pub async fn create_job(
    state: &AppState,
    ctx: &AuditCtx,
    input: CreateJob,
) -> Result<uuid::Uuid, ApiError> {
    if input.ty.bucket_required() && backup_bucket(state).is_none() {
        return Err(bucket_not_configured());
    }
    let mut params = input.params.clone();
    params.shift_remove("dryRun");
    let hash = params_hash(&params);

    let claimed = claim(state, ctx, &input, &params, &hash).await;
    let (id, uuid) = match claimed {
        Ok(v) => v,
        Err(ClaimError::Api(e)) => return Err(e),
        Err(ClaimError::Sql(e)) if mz_core::db::is_unique_violation(&e) || is_lock_timeout(&e) => {
            // Penahan DB `adminJob_one_active_per_company`, atau job Rust lain untuk company
            // yang sama sedang memegang kunci klaim (restore/merge berjalan).
            return Err(job_already_active());
        }
        Err(ClaimError::Sql(e)) => return Err(e.into()),
    };

    let enq = mz_core::queue::enqueue(
        state,
        QUEUE,
        Some(job_id(&uuid.to_string())),
        json!({ "uuid": uuid.to_string() }),
        mz_core::queue::JobOpts::new(1, Duration::ZERO),
    )
    .await;
    if let Err(err) = enq {
        tracing::error!(job = %uuid, error = %err, "Gagal mengantre admin job");
        let res = sqlx::query(
            r#"UPDATE "adminJob" SET status = 'FAILED', "finishedAt" = $2, log = $3
                WHERE id = $1 AND status = 'QUEUED'"#,
        )
        .bind(id)
        .bind(mz_core::time::now_naive())
        .bind(append_log_tail(None, &format!("Enqueue failed: {err}")))
        .execute(&state.db)
        .await;
        if let Err(e) = res {
            tracing::error!(job = %uuid, error = %e, "Gagal menandai admin job FAILED");
        }
        return Err(ApiError::coded(
            StatusCode::SERVICE_UNAVAILABLE,
            "JOB_ENQUEUE_FAILED",
            "The job could not be queued; try again later",
        ));
    }
    Ok(uuid)
}

enum ClaimError {
    Api(ApiError),
    Sql(sqlx::Error),
}

impl From<sqlx::Error> for ClaimError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sql(e)
    }
}

async fn claim(
    state: &AppState,
    ctx: &AuditCtx,
    input: &CreateJob,
    params: &Map<String, Value>,
    hash: &str,
) -> Result<(i32, uuid::Uuid), ClaimError> {
    let mut tx = state.db.begin().await?;
    sqlx::query(&format!("SET LOCAL lock_timeout = '{CLAIM_LOCK_TIMEOUT}'"))
        .execute(&mut *tx)
        .await?;
    mz_core::db::advisory_xact_lock(&mut tx, &lock_key(input.company_id)).await?;

    let active: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "adminJob"
            WHERE "companyId" IS NOT DISTINCT FROM $1 AND status IN ('QUEUED', 'RUNNING') LIMIT 1"#,
    )
    .bind(input.company_id)
    .fetch_optional(&mut *tx)
    .await?;
    if active.is_some() {
        return Err(ClaimError::Api(job_already_active()));
    }

    if !input.dry_run && input.ty.dry_run_required() {
        let since = mz_core::time::now_naive() - DRY_RUN_VALIDITY;
        let done: Option<i32> = sqlx::query_scalar(
            r#"SELECT id FROM "adminJob"
                WHERE type = $1::"AdminJobType" AND "companyId" IS NOT DISTINCT FROM $2
                  AND "paramsHash" = $3 AND "dryRun" = true AND status = 'DONE'
                  AND "finishedAt" >= $4
                LIMIT 1"#,
        )
        .bind(input.ty.as_str())
        .bind(input.company_id)
        .bind(hash)
        .bind(since)
        .fetch_optional(&mut *tx)
        .await?;
        if done.is_none() {
            return Err(ClaimError::Api(ApiError::coded(
                StatusCode::CONFLICT,
                "DRY_RUN_REQUIRED",
                "A successful dry run with the same parameters within the last 24 hours is required",
            )));
        }
    }

    let stored = strip_secrets(&Value::Object(params.clone()), 0);
    let (id, uuid): (i32, uuid::Uuid) = sqlx::query_as(
        r#"INSERT INTO "adminJob" (type, "companyId", params, "paramsHash", "dryRun", status, "operatorId")
           VALUES ($1::"AdminJobType", $2, $3, $4, $5, 'QUEUED', $6)
           RETURNING id, uuid"#,
    )
    .bind(input.ty.as_str())
    .bind(input.company_id)
    .bind(&stored)
    .bind(hash)
    .bind(input.dry_run)
    .bind(input.operator_id)
    .fetch_one(&mut *tx)
    .await?;

    let after = json!({
        "type": input.ty.as_str(),
        "dryRun": input.dry_run,
        "params": Value::Object(params.clone()),
        "runner": RUNNER_MARK,
    });
    record(
        &mut *tx,
        ctx,
        &OperatorAuditEntry {
            operator_id: input.operator_id,
            action: "admin_job.create",
            target_type: "adminJob",
            target_uuid: Some(uuid),
            company_id: input.company_id,
            before: None,
            after: Some(&after),
            reason: input.reason.as_deref(),
        },
    )
    .await?;
    tx.commit().await?;
    Ok((id, uuid))
}

// ── Endpoint ────────────────────────────────────────────────────────────────

fn created(uuid: uuid::Uuid) -> Value {
    json!({ "data": { "jobUuid": uuid.to_string(), "status": "QUEUED" } })
}

async fn find_company(state: &AppState, uuid: &str) -> Result<(i32, uuid::Uuid), ApiError> {
    let row: Option<(i32, uuid::Uuid)> =
        sqlx::query_as(r#"SELECT id, uuid FROM company WHERE uuid = $1::uuid"#)
            .bind(uuid)
            .fetch_optional(&state.db)
            .await?;
    row.ok_or_else(|| {
        ApiError::coded(
            StatusCode::NOT_FOUND,
            "COMPANY_NOT_FOUND",
            "Company not found",
        )
    })
}

/// `CreateBackupJobDto` sesudah validasi.
#[derive(Debug, Clone)]
pub struct BackupDto {
    pub company_uuid: String,
    pub skip_heavy: Option<bool>,
    pub redact_secrets: Option<bool>,
    pub reason: Option<String>,
}

/// `createBackup(dto, operatorId)`.
pub async fn create_backup(
    state: &AppState,
    ctx: &AuditCtx,
    dto: BackupDto,
    operator_id: i32,
) -> Result<Value, ApiError> {
    if backup_bucket(state).is_none() {
        return Err(bucket_not_configured());
    }
    let (company_id, company_uuid) = find_company(state, &dto.company_uuid).await?;
    let mut params = Map::new();
    params.insert("companyUuid".into(), json!(company_uuid.to_string()));
    params.insert("skipHeavy".into(), json!(dto.skip_heavy.unwrap_or(false)));
    params.insert(
        "redactSecrets".into(),
        json!(dto.redact_secrets.unwrap_or(false)),
    );
    let uuid = create_job(
        state,
        ctx,
        CreateJob {
            ty: JobType::Backup,
            company_id: Some(company_id),
            params,
            dry_run: false,
            operator_id,
            reason: dto.reason,
        },
    )
    .await?;
    Ok(created(uuid))
}

/// `CreateRestoreJobDto` sesudah validasi.
#[derive(Debug, Clone)]
pub struct RestoreDto {
    pub backup_job_uuid: String,
    pub rename: Option<String>,
    pub users: String,
    pub username_suffix: Option<String>,
    pub dry_run: bool,
    pub reason: Option<String>,
}

/// `createRestore(dto, operatorId)`.
pub async fn create_restore(
    state: &AppState,
    ctx: &AuditCtx,
    dto: RestoreDto,
    operator_id: i32,
) -> Result<Value, ApiError> {
    if backup_bucket(state).is_none() {
        return Err(bucket_not_configured());
    }
    if dto.users == "insert" && dto.username_suffix.as_deref().is_none_or(str::is_empty) {
        return Err(ApiError::coded(
            StatusCode::BAD_REQUEST,
            "USERNAME_SUFFIX_REQUIRED",
            "`usernameSuffix` is required when `users` is `insert`",
        ));
    }
    let backup: Option<(String, String, Option<i32>, Option<Value>)> = sqlx::query_as(
        r#"SELECT type::text, status::text, "companyId", result FROM "adminJob" WHERE uuid = $1::uuid"#,
    )
    .bind(&dto.backup_job_uuid)
    .fetch_optional(&state.db)
    .await?;
    let Some((ty, status, company_id, result)) = backup.filter(|b| b.0 == "BACKUP_COMPANY") else {
        return Err(ApiError::coded(
            StatusCode::NOT_FOUND,
            "BACKUP_JOB_NOT_FOUND",
            "Backup job not found",
        ));
    };
    let _ = ty;
    if status != "DONE" || backup_files_of(result.as_ref()).is_empty() {
        return Err(ApiError::coded(
            StatusCode::CONFLICT,
            "BACKUP_NOT_AVAILABLE",
            "Only a finished backup job can be restored",
        ));
    }
    let mut params = Map::new();
    params.insert("backupJobUuid".into(), json!(dto.backup_job_uuid));
    params.insert("mode".into(), json!("clone"));
    params.insert("rename".into(), json!(dto.rename));
    params.insert("users".into(), json!(dto.users));
    params.insert(
        "usernameSuffix".into(),
        if dto.users == "insert" {
            json!(dto.username_suffix)
        } else {
            Value::Null
        },
    );
    let uuid = create_job(
        state,
        ctx,
        CreateJob {
            ty: JobType::Restore,
            company_id,
            params,
            dry_run: dto.dry_run,
            operator_id,
            reason: dto.reason,
        },
    )
    .await?;
    Ok(created(uuid))
}

/// `CreateMergeJobDto` sesudah validasi.
#[derive(Debug, Clone)]
pub struct MergeDto {
    pub target_company_uuid: String,
    pub legacy_company_id: i64,
    pub prefer_legacy: Option<bool>,
    pub retention: String,
    pub since: Option<String>,
    pub until: Option<String>,
    pub dry_run: bool,
    pub reason: Option<String>,
}

/// `createMerge(dto, operatorId)`.
pub async fn create_merge(
    state: &AppState,
    ctx: &AuditCtx,
    dto: MergeDto,
    operator_id: i32,
) -> Result<Value, ApiError> {
    if legacy_database_url(state).is_none() {
        return Err(ApiError::coded(
            StatusCode::PRECONDITION_FAILED,
            "LEGACY_DATABASE_NOT_CONFIGURED",
            "LEGACY_DATABASE_URL is not configured on the server",
        ));
    }
    if let (Some(s), Some(u)) = (&dto.since, &dto.until)
        && s > u
    {
        return Err(ApiError::coded(
            StatusCode::BAD_REQUEST,
            "INVALID_DATE_RANGE",
            "`since` must be before or equal to `until`",
        ));
    }
    let (company_id, company_uuid) = find_company(state, &dto.target_company_uuid).await?;
    let mut params = Map::new();
    params.insert("targetCompanyUuid".into(), json!(company_uuid.to_string()));
    params.insert("legacyCompanyId".into(), json!(dto.legacy_company_id));
    params.insert(
        "preferLegacy".into(),
        json!(dto.prefer_legacy.unwrap_or(false)),
    );
    params.insert("retention".into(), json!(dto.retention));
    params.insert("since".into(), json!(dto.since));
    params.insert("until".into(), json!(dto.until));
    let uuid = create_job(
        state,
        ctx,
        CreateJob {
            ty: JobType::Merge,
            company_id: Some(company_id),
            params,
            dry_run: dto.dry_run,
            operator_id,
            reason: dto.reason,
        },
    )
    .await?;
    Ok(created(uuid))
}

/// `AdminJobsQueryDto` sesudah validasi.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    pub ty: Option<String>,
    pub status: Option<String>,
    pub company_uuid: Option<String>,
    pub page_number: i64,
    pub items_per_page: i64,
}

fn js_date(v: Option<NaiveDateTime>) -> Value {
    v.map_or(Value::Null, |d| Value::String(JsDate(d).to_js_string()))
}

fn ref_obj(uuid: Option<uuid::Uuid>, name: Option<String>) -> Value {
    match uuid {
        Some(u) => json!({ "uuid": u.to_string(), "name": name }),
        None => Value::Null,
    }
}

type JobRow = (
    uuid::Uuid,
    String,
    String,
    bool,
    Value,
    NaiveDateTime,
    Option<NaiveDateTime>,
    Option<NaiveDateTime>,
    uuid::Uuid,
    String,
    Option<uuid::Uuid>,
    Option<String>,
);

/// `list(query)`.
pub async fn list(state: &AppState, q: &ListQuery) -> Result<Value, ApiError> {
    let filters = r#"
        FROM "adminJob" j
        JOIN operators o ON o.id = j."operatorId"
        LEFT JOIN company c ON c.id = j."companyId"
       WHERE ($1::text IS NULL OR j.type::text = $1)
         AND ($2::text IS NULL OR j.status::text = $2)
         AND ($3::uuid IS NULL OR c.uuid = $3::uuid)"#;
    let rows: Vec<JobRow> = sqlx::query_as(&format!(
        r#"SELECT j.uuid, j.type::text, j.status::text, j."dryRun", j.params, j."createdAt",
                  j."startedAt", j."finishedAt", o.uuid, o.name, c.uuid, c.name
           {filters}
           ORDER BY j."createdAt" DESC, j.id DESC
           LIMIT $4 OFFSET $5"#
    ))
    .bind(&q.ty)
    .bind(&q.status)
    .bind(&q.company_uuid)
    .bind(q.items_per_page)
    .bind((q.page_number - 1) * q.items_per_page)
    .fetch_all(&state.db)
    .await?;
    let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) {filters}"))
        .bind(&q.ty)
        .bind(&q.status)
        .bind(&q.company_uuid)
        .fetch_one(&state.db)
        .await?;
    let data: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "uuid": r.0.to_string(),
                "type": r.1,
                "status": r.2,
                "dryRun": r.3,
                "params": r.4,
                "createdAt": JsDate(r.5).to_js_string(),
                "startedAt": js_date(r.6),
                "finishedAt": js_date(r.7),
                "operator": ref_obj(Some(r.8), Some(r.9)),
                "company": ref_obj(r.10, r.11),
            })
        })
        .collect();
    Ok(json!({
        "data": data,
        "pageNumber": q.page_number,
        "itemsPerPage": q.items_per_page,
        "total": total,
    }))
}

/// `get(uuid)`.
pub async fn get(state: &AppState, uuid: uuid::Uuid) -> Result<Value, ApiError> {
    let row: Option<sqlx::postgres::PgRow> = sqlx::query(
        r#"SELECT j.uuid, j.type::text AS type, j.status::text AS status, j."dryRun", j.params,
                  j."paramsHash", j.result, j.log, j."createdAt", j."startedAt", j."finishedAt",
                  o.uuid AS "operatorUuid", o.name AS "operatorName",
                  c.uuid AS "companyUuid", c.name AS "companyName"
             FROM "adminJob" j
             JOIN operators o ON o.id = j."operatorId"
             LEFT JOIN company c ON c.id = j."companyId"
            WHERE j.uuid = $1"#,
    )
    .bind(uuid)
    .fetch_optional(&state.db)
    .await?;
    let Some(r) = row else {
        return Err(job_not_found());
    };
    use sqlx::Row as _;
    let g = |e: sqlx::Error| ApiError::from(e);
    Ok(json!({
        "data": {
            "uuid": r.try_get::<uuid::Uuid, _>("uuid").map_err(g)?.to_string(),
            "type": r.try_get::<String, _>("type").map_err(g)?,
            "status": r.try_get::<String, _>("status").map_err(g)?,
            "dryRun": r.try_get::<bool, _>("dryRun").map_err(g)?,
            "params": r.try_get::<Value, _>("params").map_err(g)?,
            "paramsHash": r.try_get::<String, _>("paramsHash").map_err(g)?,
            "result": r.try_get::<Option<Value>, _>("result").map_err(g)?,
            "log": r.try_get::<Option<String>, _>("log").map_err(g)?,
            "createdAt": JsDate(r.try_get::<NaiveDateTime, _>("createdAt").map_err(g)?).to_js_string(),
            "startedAt": js_date(r.try_get("startedAt").map_err(g)?),
            "finishedAt": js_date(r.try_get("finishedAt").map_err(g)?),
            "operator": ref_obj(Some(r.try_get("operatorUuid").map_err(g)?), Some(r.try_get("operatorName").map_err(g)?)),
            "company": ref_obj(r.try_get("companyUuid").map_err(g)?, r.try_get("companyName").map_err(g)?),
        }
    }))
}

/// `download(uuid)`: signed read URL (15 menit) per file job BACKUP yang DONE.
pub async fn download(state: &AppState, uuid: uuid::Uuid) -> Result<Value, ApiError> {
    let row: Option<(String, String, Option<Value>)> = sqlx::query_as(
        r#"SELECT type::text, status::text, result FROM "adminJob" WHERE uuid = $1"#,
    )
    .bind(uuid)
    .fetch_optional(&state.db)
    .await?;
    let Some((ty, status, result)) = row else {
        return Err(job_not_found());
    };
    if ty != "BACKUP_COMPANY" || status != "DONE" {
        return Err(not_downloadable(
            "Only finished backup jobs can be downloaded",
        ));
    }
    let (prefix, bucket) = backup_location_of(result.as_ref());
    let Some(bucket) = bucket.or_else(|| backup_bucket(state)) else {
        return Err(bucket_not_configured());
    };
    let Some(prefix) = prefix else {
        return Err(not_downloadable("Backup job has no stored files"));
    };
    let expires_at =
        JsDate::from_millis(chrono::Utc::now().timestamp_millis() + BACKUP_URL_TTL_SECONDS * 1000)
            .map(|d| d.to_js_string())
            .unwrap_or_default();
    let mut files = Vec::new();
    for f in backup_files_of(result.as_ref()) {
        let url = mz_files::get_signed_read_url(
            state,
            &format!("{prefix}{}", f.name),
            BACKUP_URL_TTL_SECONDS,
            Some(&bucket),
        )
        .await
        .map_err(ApiError::internal)?;
        files.push(json!({ "name": f.name, "url": url, "expiresAt": expires_at }));
    }
    Ok(json!({ "data": { "files": files } }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn hash_tidak_bergantung_urutan_kunci() {
        let a = params_hash(
            json!({ "a": 1, "b": { "x": 1, "y": [1, 2] }, "c": "z" })
                .as_object()
                .unwrap(),
        );
        let b = params_hash(
            json!({ "c": "z", "b": { "y": [1, 2], "x": 1 }, "a": 1 })
                .as_object()
                .unwrap(),
        );
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn hash_mengabaikan_dry_run_dan_peka_nilai() {
        let h = |v: Value| params_hash(v.as_object().unwrap());
        assert_eq!(
            h(json!({ "a": 1, "dryRun": true })),
            h(json!({ "a": 1, "dryRun": false }))
        );
        assert_eq!(h(json!({ "a": 1, "dryRun": true })), h(json!({ "a": 1 })));
        assert_ne!(h(json!({ "a": 1 })), h(json!({ "a": 2 })));
        assert_ne!(h(json!({ "a": [1, 2] })), h(json!({ "a": [2, 1] })));
    }

    #[test]
    fn canonical_json_mengurutkan_kunci() {
        assert_eq!(
            canonical_json(&json!({ "b": 1, "c": null })),
            r#"{"b":1,"c":null}"#
        );
    }

    /// Hash sama dengan Node (`adminJobParamsHash` untuk params restore spec Node).
    #[test]
    fn hash_sama_dengan_node() {
        let p = json!({
            "backupJobUuid": "33333333-3333-4333-8333-333333333333", "mode": "clone", "users": "reuse"
        });
        assert_eq!(
            canonical_json(&p),
            r#"{"backupJobUuid":"33333333-3333-4333-8333-333333333333","mode":"clone","users":"reuse"}"#
        );
    }

    #[test]
    fn append_log_tail_menyisakan_ekor() {
        let long = "x".repeat(LOG_CAP);
        let out = append_log_tail(Some(&long), "baris terakhir");
        assert_eq!(out.encode_utf16().count(), LOG_CAP);
        assert!(out.ends_with("baris terakhir\n"));
    }

    #[test]
    fn log_tail_menyimpan_ekor_sepanjang_cap() {
        let mut t = LogTail::default();
        assert!(!t.take_dirty());
        t.append("a");
        assert!(t.take_dirty());
        assert!(!t.take_dirty());
        let line = "y".repeat(1000);
        for _ in 0..500 {
            t.append(&line);
        }
        assert!(t.value().encode_utf16().count() <= LOG_CAP);
        assert!(t.value().ends_with(&format!("{line}\n")));
    }

    #[test]
    fn backup_files_dan_lokasi() {
        let r = json!({
            "bucket": "b", "prefix": "company-backups/x/",
            "files": [{ "name": "manifest.json", "size": 10 }, { "size": 1 }, "x", { "name": "data/u.ndjson.gz" }]
        });
        let files = backup_files_of(Some(&r));
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].size, 0.0);
        assert_eq!(
            backup_location_of(Some(&r)),
            (Some("company-backups/x/".into()), Some("b".into()))
        );
        assert!(backup_files_of(None).is_empty());
        assert_eq!(backup_location_of(Some(&json!([1]))), (None, None));
    }

    #[test]
    fn kunci_dan_timeout() {
        assert_eq!(lock_key(Some(5)), "admin-job:5");
        assert_eq!(lock_key(None), "admin-job:global");
        assert_eq!(job_id("u"), "admin-job:u");
        assert_eq!(JobType::Merge.timeout(), Duration::from_secs(180 * 60));
        assert_eq!(
            JobType::ApplyHolidays.timeout(),
            Duration::from_secs(20 * 60)
        );
        assert!(JobType::Restore.dry_run_required() && !JobType::Backup.dry_run_required());
        assert!(JobType::Backup.bucket_required() && !JobType::Merge.bucket_required());
    }
}
