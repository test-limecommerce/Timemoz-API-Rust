//! Port `products/product-import.consumer.ts`: worker antrean `product-import` (`mz_core::queue`).
//! Status yang dipercaya = baris `productImportJob` (bukan state antrean).

use std::collections::HashSet;
use std::time::Duration;

use futures::StreamExt;
use mz_audit::{AuditHandle, Lookup, Model};
use mz_core::AppState;
use mz_core::queue::{self, Job, Outcome, WorkerConfig};
use mz_core::time::now_naive;
use serde_json::{Value, json};
use sqlx::types::Json;
use tokio::task::JoinHandle;

use super::columns::{
    IMAGE_DOWNLOAD_CONCURRENCY, IMPORT_BATCH_SIZE, MAX_STORED_IMPORT_ISSUES,
    PRODUCT_IMPORT_JOB_TIMEOUT, PRODUCT_IMPORT_QUEUE,
};
use super::parser::{ImportParseError, parse_default};
use super::row::{MasterMaps, ResolvedImportRow, RowIssue, resolve_import_row};
use super::safe_fetch::{BlockFn, download_image_safely_with};
use super::writer::{WrittenProduct, describe_batch_error, write_import_batch};
use crate::custom_fields_validator::CfDefinition;
use crate::masters::{MasterKind, MasterRow, list_company};
use crate::util::{extension_for_content_type, is_blocked_ip, product_image_path};

/// Baris `productImportJob` yang dibutuhkan proses.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ImportJobRow {
    pub id: i32,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    #[sqlx(rename = "createdById")]
    pub created_by_id: i32,
    #[sqlx(rename = "objectKey")]
    pub object_key: String,
    #[sqlx(rename = "dryRun")]
    pub dry_run: bool,
}

/// Worker antrean `product-import`: konkurensi 1, timeout handler 15 menit (`PRODUCT_IMPORT_JOB_TIMEOUT_MS`),
/// lease 16 menit. Dipasang orkestrator bersama worker lain (`MZ_JOBS`).
pub fn spawn_worker(state: &AppState) -> JoinHandle<()> {
    let cfg = WorkerConfig {
        concurrency: 1,
        lease: PRODUCT_IMPORT_JOB_TIMEOUT + Duration::from_secs(60),
        handler_timeout: PRODUCT_IMPORT_JOB_TIMEOUT,
        poll: queue::DEFAULT_POLL,
    };
    let st = state.clone();
    queue::spawn_worker_with(state, PRODUCT_IMPORT_QUEUE, cfg, move |job: Job| {
        let st = st.clone();
        async move {
            handle(&st, &job.data).await;
            Outcome::Done
        }
    })
}

/// `handle(job)`: `jobId` harus bilangan bulat > 0; baris tidak ada → dilewati.
pub async fn handle(state: &AppState, data: &Value) {
    let id = data
        .get("jobId")
        .and_then(Value::as_f64)
        .unwrap_or(f64::NAN);
    if !(id.is_finite() && id.fract() == 0.0 && id > 0.0 && id <= f64::from(i32::MAX)) {
        return;
    }
    #[allow(clippy::cast_possible_truncation)]
    let id = id as i32;
    if let Err(e) = process_job(state, id).await {
        tracing::error!(job = id, error = %e, "import produk gagal (DB)");
    }
}

/// Proses satu baris job (dipanggil worker; publik untuk uji/orkestrator).
pub async fn process_job(state: &AppState, job_id: i32) -> anyhow::Result<()> {
    process_job_with(state, job_id, is_blocked_ip).await
}

/// Seperti [`process_job`] dengan penolak IP unduhan gambar yang bisa disuntik (HANYA uji).
pub async fn process_job_with(
    state: &AppState,
    job_id: i32,
    is_blocked: BlockFn,
) -> anyhow::Result<()> {
    let row: Option<ImportJobRow> = sqlx::query_as(
        r#"SELECT id, "companyId", "createdById", "objectKey", "dryRun" FROM "productImportJob" WHERE id = $1"#,
    )
    .bind(job_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else {
        tracing::warn!("productImportJob {job_id} not found — skipping");
        return Ok(());
    };
    process(state, &row, is_blocked).await
}

async fn master_map(
    state: &AppState,
    kind: MasterKind,
    company_id: i32,
) -> anyhow::Result<std::collections::HashMap<String, MasterRow>> {
    let rows = list_company(&state.db, kind, company_id, 10_000)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(rows.into_iter().map(|r| (r.code.clone(), r)).collect())
}

async fn build_master_maps(state: &AppState, company_id: i32) -> anyhow::Result<MasterMaps> {
    Ok(MasterMaps {
        category: master_map(state, MasterKind::Categories, company_id).await?,
        sub_category: master_map(state, MasterKind::SubCategories, company_id).await?,
        brand: master_map(state, MasterKind::Brands, company_id).await?,
        principal: master_map(state, MasterKind::Principals, company_id).await?,
        uom: master_map(state, MasterKind::Uoms, company_id).await?,
        tax: master_map(state, MasterKind::Taxes, company_id).await?,
    })
}

/// Definisi custom field produk (aktif + arsip, maks 1000) — untuk validasi tulis.
pub async fn custom_field_definitions(
    db: &sqlx::PgPool,
    company_id: i32,
) -> sqlx::Result<Vec<CfDefinition>> {
    let rows: Vec<(String, String, bool, bool, Option<Value>)> = sqlx::query_as(
        r#"SELECT key, type::text, "isRequired", "isActive", options FROM "productCustomField"
           WHERE "companyId" = $1 AND entity = 'product'::"CustomFieldEntity" ORDER BY id LIMIT 1000"#,
    )
    .bind(company_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(key, field_type, is_required, is_active, options)| CfDefinition {
                key,
                field_type,
                is_required,
                is_active,
                options: CfDefinition::options_from_json(options.as_ref()),
            },
        )
        .collect())
}

fn truncated(v: &[RowIssue]) -> Json<&[RowIssue]> {
    Json(v.get(..MAX_STORED_IMPORT_ISSUES).unwrap_or(v))
}

async fn save_progress(
    state: &AppState,
    job_id: i32,
    succeeded: usize,
    failed: usize,
    errors: &[RowIssue],
    warnings: &[RowIssue],
) -> sqlx::Result<()> {
    sqlx::query(
        r#"UPDATE "productImportJob" SET processed = $2, succeeded = $3, failed = $4, errors = $5,
             warnings = $6, "updatedAt" = $7 WHERE id = $1"#,
    )
    .bind(job_id)
    .bind(i32::try_from(succeeded + failed).unwrap_or(i32::MAX))
    .bind(i32::try_from(succeeded).unwrap_or(i32::MAX))
    .bind(i32::try_from(failed).unwrap_or(i32::MAX))
    .bind(truncated(errors))
    .bind(truncated(warnings))
    .bind(now_naive())
    .execute(&state.db)
    .await?;
    Ok(())
}

/// §8.2 — unduh gambar SESUDAH commit, konkurensi 5; gagal = `warnings`.
async fn download_images_for_batch(
    state: &AppState,
    audit: &AuditHandle,
    company_id: i32,
    written: &[WrittenProduct],
    warnings: &mut Vec<RowIssue>,
    is_blocked: BlockFn,
) {
    let with_images: Vec<&WrittenProduct> = written
        .iter()
        .filter(|w| w.row.image_url.is_some())
        .collect();
    if with_images.is_empty() {
        return;
    }
    let futs: Vec<_> = with_images
        .into_iter()
        .map(|w| image_issue(state, audit, company_id, w, is_blocked))
        .collect();
    let results: Vec<Option<RowIssue>> = futures::stream::iter(futs)
        .buffer_unordered(IMAGE_DOWNLOAD_CONCURRENCY)
        .collect()
        .await;
    warnings.extend(results.into_iter().flatten());
}

async fn image_issue(
    state: &AppState,
    audit: &AuditHandle,
    company_id: i32,
    w: &WrittenProduct,
    is_blocked: BlockFn,
) -> Option<RowIssue> {
    match store_image(state, audit, company_id, w, is_blocked).await {
        Ok(()) => None,
        Err((code, message)) => Some(RowIssue::new(
            w.row.row_number,
            Some("imageUrl"),
            code,
            message,
        )),
    }
}

async fn store_image(
    state: &AppState,
    audit: &AuditHandle,
    company_id: i32,
    w: &WrittenProduct,
    is_blocked: BlockFn,
) -> Result<(), (&'static str, String)> {
    let url = w.row.image_url.as_deref().unwrap_or_default();
    let img = download_image_safely_with(url, is_blocked)
        .await
        .map_err(|e| (e.code, e.message))?;
    let ext = extension_for_content_type(&img.content_type).unwrap_or("jpg");
    let key = format!(
        "products/{company_id}/{}/{}.{ext}",
        w.product_uuid,
        uuid::Uuid::new_v4()
    );
    mz_files::upload_buffer(state, &key, img.buffer, &img.content_type)
        .await
        .map_err(|e| ("IMAGE_DOWNLOAD_FAILED", e.to_string()))?;
    let now = now_naive();
    let pre = audit
        .pre_read(Model::Product, Lookup::Id(w.product_id))
        .await;
    sqlx::query(
        r#"UPDATE product SET "imageUrl" = $2, "imageObjectKey" = $3, "imageUpdatedAt" = $4, "updatedAt" = $4
           WHERE id = $1"#,
    )
    .bind(w.product_id)
    .bind(product_image_path(&w.product_uuid, Some(&key)))
    .bind(&key)
    .bind(now)
    .execute(&state.db)
    .await
    .map_err(|e| ("IMAGE_DOWNLOAD_FAILED", e.to_string()))?;
    if let Some(after) = audit
        .result_row(&state.db, Model::Product, Lookup::Id(w.product_id))
        .await
    {
        audit.record_update(Model::Product, pre, &after).await;
    }
    Ok(())
}

enum Failure {
    Parse(ImportParseError),
    Other(String),
}

async fn process(state: &AppState, job: &ImportJobRow, is_blocked: BlockFn) -> anyhow::Result<()> {
    let started = now_naive();
    sqlx::query(
        r#"UPDATE "productImportJob" SET status = 'running'::"ProductImportStatus", "startedAt" = $2,
             "updatedAt" = $2 WHERE id = $1"#,
    )
    .bind(job.id)
    .bind(started)
    .execute(&state.db)
    .await?;

    if let Err(f) = run(state, job, is_blocked).await {
        let (code, message) = match f {
            Failure::Parse(p) => (p.code, p.message),
            Failure::Other(m) => ("IMPORT_JOB_FAILED", m),
        };
        tracing::error!("Import job {} failed: {message}", job.id);
        let now = now_naive();
        sqlx::query(
            r#"UPDATE "productImportJob" SET status = 'failed'::"ProductImportStatus", errors = $2,
                 "finishedAt" = $3, "updatedAt" = $3 WHERE id = $1"#,
        )
        .bind(job.id)
        .bind(Json(
            json!([{ "row": 0, "column": null, "code": code, "message": message }]),
        ))
        .bind(now)
        .execute(&state.db)
        .await?;
    }
    Ok(())
}

async fn run(state: &AppState, job: &ImportJobRow, is_blocked: BlockFn) -> Result<(), Failure> {
    let other = |e: &dyn std::fmt::Display| Failure::Other(e.to_string());
    let buffer = mz_files::download_buffer(state, &job.object_key)
        .await
        .map_err(|e| other(&e))?;
    let parsed = parse_default(&buffer).map_err(Failure::Parse)?;
    let masters = build_master_maps(state, job.company_id)
        .await
        .map_err(|e| other(&e))?;
    let defs = custom_field_definitions(&state.db, job.company_id)
        .await
        .map_err(|e| other(&e))?;
    // Node: job Bull tanpa konteks request → entri audit `system`, ditulis langsung.
    let audit = AuditHandle::detached(state.db.clone());

    let mut seen: HashSet<String> = HashSet::new();
    let mut errors: Vec<RowIssue> = Vec::new();
    let mut warnings: Vec<RowIssue> = Vec::new();
    let mut resolved: Vec<ResolvedImportRow> = Vec::new();
    let mut failed = 0usize;
    for r in &parsed.rows {
        let res = resolve_import_row(r.row_number, &r.values, &parsed, &masters, &defs, &mut seen);
        match res.data {
            Some(d) => resolved.push(d),
            None => failed += 1,
        }
        errors.extend(res.issues);
    }

    let mut succeeded = 0usize;
    for batch in resolved.chunks(IMPORT_BATCH_SIZE) {
        let outcome = write_batch(state, &audit, job, batch).await;
        match outcome {
            Ok(written) => {
                succeeded += written.len();
                if !job.dry_run {
                    download_images_for_batch(
                        state,
                        &audit,
                        job.company_id,
                        &written,
                        &mut warnings,
                        is_blocked,
                    )
                    .await;
                }
            }
            Err(e) => {
                failed += batch.len();
                let (code, message) = describe_batch_error(&e);
                for row in batch {
                    errors.push(RowIssue::new(row.row_number, None, code, message.clone()));
                }
                tracing::error!("Import batch failed for job {}: {message}", job.id);
            }
        }
        save_progress(state, job.id, succeeded, failed, &errors, &warnings)
            .await
            .map_err(|e| other(&e))?;
    }

    let now = now_naive();
    sqlx::query(
        r#"UPDATE "productImportJob" SET status = 'completed'::"ProductImportStatus", "totalRows" = $2,
             processed = $3, succeeded = $4, failed = $5, errors = $6, warnings = $7,
             "finishedAt" = $8, "updatedAt" = $8 WHERE id = $1"#,
    )
    .bind(job.id)
    .bind(i32::try_from(parsed.rows.len()).unwrap_or(i32::MAX))
    .bind(i32::try_from(succeeded + failed).unwrap_or(i32::MAX))
    .bind(i32::try_from(succeeded).unwrap_or(i32::MAX))
    .bind(i32::try_from(failed).unwrap_or(i32::MAX))
    .bind(truncated(&errors))
    .bind(truncated(&warnings))
    .bind(now)
    .execute(&state.db)
    .await
    .map_err(|e| other(&e))?;
    Ok(())
}

/// Satu batch dalam satu transaksi; dryRun = rollback sesudah semua tulisan lolos.
async fn write_batch(
    state: &AppState,
    audit: &AuditHandle,
    job: &ImportJobRow,
    batch: &[ResolvedImportRow],
) -> sqlx::Result<Vec<WrittenProduct>> {
    let mut tx = state.db.begin().await?;
    let result =
        write_import_batch(&mut tx, audit, job.company_id, job.created_by_id, batch).await?;
    if job.dry_run {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(result.written)
}
