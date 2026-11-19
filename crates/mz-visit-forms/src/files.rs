//! Port `store-visit-form-files.service.ts` (§6.5 siklus hidup file tanda tangan, foto,
//! lampiran, dan aset kop/kaki PDF) + `mobile-store-visit-form-file.controller.ts`
//! (`GET /v1/mobile/store-visit-forms/submissions/:uuid/files/:fileUuid`, tanpa JWT).
//!
//! Logika murni (aturan node media, objectKey, rencana rekonsiliasi, pemilihan media PDF,
//! pengecilan gambar) dipisah dari I/O supaya bisa diuji tanpa DB/GCS.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::Utc;
use mz_core::jsfmt::{num, num_string};
use mz_core::permissions::Exempt;
use mz_core::rate_limit::{RateKey, RateRule};
use mz_core::uuid_rules::{is_uuid_shape, uuid_pipe};
use mz_core::{ApiError, ApiResult, AppState, RouteSpec};
use mz_files::{SignedObjectOpts, SignedReadUrl, StreamObjectOptions};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::answers_validator::{AnswerValidationMode, ValidateAnswersOptions, validate_answers};
use crate::consts::{FormNodeType, MAX_FILE_BYTES, MAX_LAYOUT_ASSET_BYTES};
use crate::errors::{coded_details, not_found, unprocessable, unprocessable_details};
use crate::file_url::{admin_form_file_path, mobile_form_file_path, signed_mobile_form_file_url};
use crate::layout_validator::collect_layout_asset_references;
use crate::node_path::{find_node_by_field_path, set_value_at_field_path};
use crate::types::{AnswerFileReference, FormDocument, FormNode, PdfMedia};

/// URL tulis 15 menit (`UPLOAD_URL_TTL_SECONDS`).
pub const UPLOAD_URL_TTL_SECONDS: i64 = 900;
/// Baris belum terkonfirmasi selewat ini disapu cron.
pub const UNCONFIRMED_MAX_AGE_HOURS: i64 = 24;
/// Plafon media satu ekspor PDF.
pub const PDF_MEDIA_MAX_FILES: usize = 40;
pub const PDF_MEDIA_MAX_TOTAL_BYTES: i64 = 25 * 1024 * 1024;
pub const PDF_MEDIA_MAX_WIDTH: u32 = 1400;
/// Di bawah ini decode lebih mahal daripada byte yang dihematnya.
const PDF_MEDIA_SHRINK_THRESHOLD_BYTES: usize = 300 * 1024;
const PDF_MEDIA_BATCH_SIZE: usize = 5;

const FORM_FILE_NOT_FOUND: (&str, &str) = ("FORM_FILE_NOT_FOUND", "File not found");
const FORM_ASSET_NOT_FOUND: (&str, &str) = ("FORM_ASSET_NOT_FOUND", "Template asset not found");

fn file_not_found() -> ApiError {
    not_found(FORM_FILE_NOT_FOUND.0, FORM_FILE_NOT_FOUND.1)
}

fn asset_not_found() -> ApiError {
    not_found(FORM_ASSET_NOT_FOUND.0, FORM_ASSET_NOT_FOUND.1)
}

/// `EXTENSION_BY_CONTENT_TYPE.get(ct) ?? 'bin'`.
pub fn extension_for(content_type: &str) -> &'static str {
    match content_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/heic" => "heic",
        "application/pdf" => "pdf",
        _ => "bin",
    }
}

/// `store-visit-forms/<companyId>/<visitUuid>/<uuid>.<ext>` (store-visit-form-files.service.ts:198).
pub fn form_file_object_key(
    company_id: i32,
    visit_uuid: &str,
    uuid: &str,
    content_type: &str,
) -> String {
    format!(
        "store-visit-forms/{company_id}/{visit_uuid}/{uuid}.{}",
        extension_for(content_type)
    )
}

/// `templateAssetPrefix` (store-visit-form-files.service.ts:1169).
pub fn template_asset_prefix(company_id: i32, template_uuid: &str) -> String {
    format!(
        "store-visit-forms/{company_id}/templates/{}/",
        template_uuid.to_lowercase()
    )
}

/// `storedTemplateAssetKey` — key sah DAN di folder template-nya.
pub fn stored_template_asset_key(
    bucket: Option<&str>,
    object_key: &str,
    company_id: i32,
    template_uuid: &str,
) -> Option<String> {
    let key = mz_files::to_object_key(object_key, bucket).ok()?;
    key.starts_with(&template_asset_prefix(company_id, template_uuid))
        .then_some(key)
}

/// `storedFormFileKey` — key sah DAN di folder kunjungannya sendiri.
pub fn stored_form_file_key(
    bucket: Option<&str>,
    object_key: &str,
    company_id: i32,
    visit_uuid: &str,
) -> Option<String> {
    let key = mz_files::to_object_key(object_key, bucket).ok()?;
    let prefix = format!("store-visit-forms/{company_id}/{visit_uuid}/");
    key.starts_with(&prefix).then_some(key)
}

fn bucket(state: &AppState) -> Option<String> {
    state.config.var("GCP_BUCKET_NAME")
}

// ── Aturan per node media ────────────────────────────────────────────────────

/// `resolveMediaNode` (store-visit-form-files.service.ts:108).
pub fn resolve_media_node<'a>(
    schema: &'a FormDocument,
    field_path: &str,
) -> ApiResult<&'a FormNode> {
    match find_node_by_field_path(schema, field_path) {
        Some(node)
            if matches!(
                node.ty,
                FormNodeType::Signature | FormNodeType::Photo | FormNodeType::File
            ) =>
        {
            Ok(node)
        }
        _ => Err(unprocessable(
            "FORM_FIELD_NOT_MEDIA",
            format!("\"{field_path}\" is not a signature, photo, or file field in this form"),
        )),
    }
}

/// `maxBytesFor` — terkecil antara plafon node dan 2 MB.
pub fn max_bytes_for(node: &FormNode) -> f64 {
    let hard = MAX_FILE_BYTES as f64;
    node.validation
        .as_ref()
        .and_then(|v| v.max_size_bytes)
        .unwrap_or(hard)
        .min(hard)
}

/// `assertContentTypeAllowed` (store-visit-form-files.service.ts:129).
pub fn assert_content_type_allowed(node: &FormNode, content_type: &str) -> ApiResult<()> {
    if node.ty == FormNodeType::Signature && content_type != "image/png" {
        return Err(unprocessable(
            "FORM_FILE_TYPE_REJECTED",
            "A signature must be uploaded as image/png",
        ));
    }
    if node.ty == FormNodeType::Photo && !content_type.starts_with("image/") {
        return Err(unprocessable(
            "FORM_FILE_TYPE_REJECTED",
            "A photo must be an image",
        ));
    }
    let Some(accept) = node.validation.as_ref().and_then(|v| v.accept.as_ref()) else {
        return Ok(());
    };
    if accept.is_empty() {
        return Ok(());
    }
    let allowed = accept.iter().any(|entry| {
        if entry == content_type {
            return true;
        }
        match entry.strip_suffix('*') {
            Some(prefix) if entry.ends_with("/*") => content_type.starts_with(prefix),
            _ => false,
        }
    });
    if allowed {
        return Ok(());
    }
    Err(unprocessable_details(
        "FORM_FILE_TYPE_REJECTED",
        format!("\"{content_type}\" is not accepted by this field"),
        json!({ "accept": accept }),
    ))
}

fn too_large(prefix: &str, max: f64) -> ApiError {
    unprocessable_details(
        "FORM_FILE_TOO_LARGE",
        format!(
            "{prefix} exceeds the {} byte limit for this field",
            num_string(max)
        ),
        json!({ "maxSizeBytes": num(max) }),
    )
}

// ── Langkah 1 §6.5 — daftarkan baris & tanda tangani URL tulis ───────────────

/// Isi `CreateFormFileUploadUrlDto` yang sudah lolos ValidationPipe.
#[derive(Debug, Clone)]
pub struct UploadRequest {
    pub field_path: String,
    pub content_type: String,
    /// Bilangan bulat 1..=2 MB (dijamin DTO).
    pub size_bytes: i64,
}

/// Hasil `registerUpload`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredUpload {
    pub uuid: String,
    pub object_key: String,
    pub content_type: String,
}

/// Bagian murni `registerUpload`: node, tipe, ukuran → objectKey.
pub fn plan_upload(
    schema: &FormDocument,
    company_id: i32,
    visit_uuid: &str,
    req: &UploadRequest,
    uuid: &str,
) -> ApiResult<RegisteredUpload> {
    let node = resolve_media_node(schema, &req.field_path)?;
    assert_content_type_allowed(node, &req.content_type)?;
    let max = max_bytes_for(node);
    if req.size_bytes as f64 > max {
        return Err(too_large("File", max));
    }
    Ok(RegisteredUpload {
        uuid: uuid.to_owned(),
        object_key: form_file_object_key(company_id, visit_uuid, uuid, &req.content_type),
        content_type: req.content_type.clone(),
    })
}

/// `registerUpload(tx, params)` (store-visit-form-files.service.ts:171) — di DALAM transaksi
/// §6.7 (baris submission draft sudah dipastikan ada). URL ditandatangani sesudah commit
/// lewat [`sign_upload_url`].
pub async fn register_upload(
    tx: &mut PgConnection,
    submission_id: i32,
    company_id: i32,
    visit_uuid: &str,
    schema: &FormDocument,
    req: &UploadRequest,
) -> ApiResult<RegisteredUpload> {
    let uuid = uuid::Uuid::new_v4().to_string();
    let planned = plan_upload(schema, company_id, visit_uuid, req, &uuid)?;
    sqlx::query(
        r#"INSERT INTO "storeVisitFormFile"
             (uuid, "submissionId", "fieldPath", "objectKey", "contentType", "sizeBytes", "isConfirmed")
           VALUES ($1::uuid, $2, $3, $4, $5, $6, false)"#,
    )
    .bind(&planned.uuid)
    .bind(submission_id)
    .bind(&req.field_path)
    .bind(&planned.object_key)
    .bind(&req.content_type)
    .bind(i32::try_from(req.size_bytes).unwrap_or(i32::MAX))
    .execute(&mut *tx)
    .await?;
    Ok(planned)
}

/// `signUploadUrl(objectKey, contentType)` (store-visit-form-files.service.ts:215).
pub async fn sign_upload_url(
    state: &AppState,
    object_key: &str,
    content_type: &str,
) -> ApiResult<String> {
    let signed =
        mz_files::get_signed_file_url(state, object_key, UPLOAD_URL_TTL_SECONDS, content_type)
            .await?;
    Ok(signed.url)
}

// ── Langkah 3 §6.5 — konfirmasi ──────────────────────────────────────────────

#[derive(Debug, Clone, sqlx::FromRow)]
struct FileRow {
    id: i32,
    uuid: String,
    field_path: Option<String>,
    object_key: String,
    content_type: String,
    size_bytes: Option<i32>,
    is_confirmed: bool,
}

const FILE_COLUMNS: &str = r#"id, uuid::text AS uuid, "fieldPath" AS field_path, "objectKey" AS object_key,
    "contentType" AS content_type, "sizeBytes" AS size_bytes, "isConfirmed" AS is_confirmed"#;

/// `toFileResponse` — `objectKey` tidak pernah keluar dari server.
pub fn to_file_response(
    uuid: &str,
    field_path: Option<&str>,
    content_type: &str,
    size_bytes: Option<i32>,
    is_confirmed: bool,
) -> Value {
    json!({
        "fileUuid": uuid,
        "fieldPath": field_path,
        "contentType": content_type,
        "sizeBytes": size_bytes,
        "isConfirmed": is_confirmed,
    })
}

fn file_response(f: &FileRow) -> Value {
    to_file_response(
        &f.uuid,
        f.field_path.as_deref(),
        &f.content_type,
        f.size_bytes,
        f.is_confirmed,
    )
}

fn size_i32(v: u64) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

/// `confirmFile({ submissionId, schema, fileUuid })` (store-visit-form-files.service.ts:232).
/// Baca balik objeknya dari bucket sebelum menandai terkonfirmasi; idempoten.
///
/// `fileUuid` yang bukan berbentuk uuid dijawab 404 `FORM_FILE_NOT_FOUND` (Node: galat Prisma).
pub async fn confirm_file(
    state: &AppState,
    submission_id: i32,
    schema: &FormDocument,
    file_uuid: &str,
) -> ApiResult<Value> {
    let not_found_for_form = || not_found("FORM_FILE_NOT_FOUND", "File not found for this form");
    if !is_uuid_shape(file_uuid) {
        return Err(not_found_for_form());
    }
    let file: Option<FileRow> = sqlx::query_as(&format!(
        r#"SELECT {FILE_COLUMNS} FROM "storeVisitFormFile"
            WHERE uuid = $1::uuid AND "submissionId" = $2 LIMIT 1"#
    ))
    .bind(file_uuid)
    .bind(submission_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(file) = file else {
        return Err(not_found_for_form());
    };
    if file.is_confirmed {
        return Ok(file_response(&file));
    }

    let metadata = mz_files::get_object_metadata(state, &file.object_key).await?;
    if !metadata.exists {
        return Err(unprocessable(
            "FORM_FILE_NOT_UPLOADED",
            "No uploaded object was found for this file",
        ));
    }
    let node = file
        .field_path
        .as_deref()
        .and_then(|p| find_node_by_field_path(schema, p));
    let max = node.map_or(MAX_FILE_BYTES as f64, max_bytes_for);
    if let Some(size) = metadata.size_bytes
        && size as f64 > max
    {
        return Err(too_large("Uploaded file", max));
    }

    let size_bytes = metadata.size_bytes.map(size_i32).or(file.size_bytes);
    let updated = sqlx::query(
        r#"UPDATE "storeVisitFormFile" SET "isConfirmed" = true, "sizeBytes" = $2
            WHERE id = $1 AND "isConfirmed" = false"#,
    )
    .bind(file.id)
    .bind(size_bytes)
    .execute(&state.db)
    .await?
    .rows_affected();

    if updated == 0 {
        let current: Option<FileRow> = sqlx::query_as(&format!(
            r#"SELECT {FILE_COLUMNS} FROM "storeVisitFormFile" WHERE id = $1"#
        ))
        .bind(file.id)
        .fetch_optional(&state.db)
        .await?;
        return current
            .map(|c| file_response(&c))
            .ok_or_else(not_found_for_form);
    }
    Ok(file_response(&FileRow {
        is_confirmed: true,
        size_bytes,
        ..file
    }))
}

// ── §6.5 — rekonsiliasi saat submit ──────────────────────────────────────────

/// Satu baris `storeVisitFormFile` sebagaimana dibaca rekonsiliasi.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct StoredFile {
    pub id: i32,
    pub uuid: String,
    pub object_key: String,
    pub is_confirmed: bool,
    pub field_path: Option<String>,
}

/// Rencana rekonsiliasi (bagian murni `reconcileOnSubmit`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReconcilePlan {
    /// id baris yatim yang dihapus.
    pub orphan_ids: Vec<i32>,
    /// objectKey yatim — dihapus dari bucket SESUDAH commit.
    pub orphan_keys: Vec<String>,
    /// `(id, fieldPath baru)` berurutan seperti `used`.
    pub path_updates: Vec<(i32, String)>,
}

/// Bagian murni `reconcileOnSubmit` (store-visit-form-files.service.ts:345).
pub fn plan_reconcile(
    stored: &[StoredFile],
    used: &[AnswerFileReference],
) -> ApiResult<ReconcilePlan> {
    let by_uuid: HashMap<&str, &StoredFile> = stored.iter().map(|r| (r.uuid.as_str(), r)).collect();
    let unconfirmed: Vec<Value> = used
        .iter()
        .filter(|r| {
            !by_uuid
                .get(r.file_uuid.as_str())
                .is_some_and(|row| row.is_confirmed)
        })
        .map(|r| json!({ "fieldPath": r.field_path, "fileUuid": r.file_uuid }))
        .collect();
    if !unconfirmed.is_empty() {
        return Err(coded_details(
            StatusCode::CONFLICT,
            "FORM_FILE_NOT_CONFIRMED",
            "Every file referenced in the answers must belong to this submission and be confirmed",
            json!({ "files": unconfirmed }),
        ));
    }
    let used_uuids: HashSet<&str> = used.iter().map(|r| r.file_uuid.as_str()).collect();
    let orphans: Vec<&StoredFile> = stored
        .iter()
        .filter(|r| !used_uuids.contains(r.uuid.as_str()))
        .collect();
    let path_updates = used
        .iter()
        .filter_map(|r| {
            let row = by_uuid.get(r.file_uuid.as_str())?;
            (row.field_path.as_deref() != Some(r.field_path.as_str()))
                .then(|| (row.id, r.field_path.clone()))
        })
        .collect();
    Ok(ReconcilePlan {
        orphan_ids: orphans.iter().map(|r| r.id).collect(),
        orphan_keys: orphans.iter().map(|r| r.object_key.clone()).collect(),
        path_updates,
    })
}

/// `reconcileOnSubmit(tx, submissionId, used)` — di dalam transaksi submit, berbasis
/// `fileUuid`. Mengembalikan objectKey yang harus dihapus dari bucket SESUDAH commit
/// (pakai [`delete_objects_quietly`]).
pub async fn reconcile_on_submit(
    tx: &mut PgConnection,
    submission_id: i32,
    used: &[AnswerFileReference],
) -> ApiResult<Vec<String>> {
    let stored: Vec<StoredFile> = sqlx::query_as(
        r#"SELECT id, uuid::text AS uuid, "objectKey" AS object_key,
                  "isConfirmed" AS is_confirmed, "fieldPath" AS field_path
             FROM "storeVisitFormFile" WHERE "submissionId" = $1 ORDER BY id"#,
    )
    .bind(submission_id)
    .fetch_all(&mut *tx)
    .await?;
    let plan = plan_reconcile(&stored, used)?;
    if !plan.orphan_ids.is_empty() {
        sqlx::query(r#"DELETE FROM "storeVisitFormFile" WHERE id = ANY($1)"#)
            .bind(&plan.orphan_ids)
            .execute(&mut *tx)
            .await?;
    }
    for (id, path) in &plan.path_updates {
        sqlx::query(r#"UPDATE "storeVisitFormFile" SET "fieldPath" = $2 WHERE id = $1"#)
            .bind(id)
            .bind(path)
            .execute(&mut *tx)
            .await?;
    }
    Ok(plan.orphan_keys)
}

/// `deleteObjectsQuietly` — tidak pernah gagal (warn saja).
pub async fn delete_objects_quietly(state: &AppState, object_keys: &[String]) {
    for key in object_keys {
        if let Err(e) = mz_files::delete_file(state, key).await {
            tracing::warn!("failed to delete bucket object {key}: {e}");
        }
    }
}

// ── §2.4 — perkaya nilai media saat dibaca ───────────────────────────────────

/// `audience` `enrichMediaValues`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    /// Path proxy relatif ber-JWT.
    Admin,
    /// URL proxy absolut bertanda tangan `FILE_URL_SECRET`.
    Mobile,
}

fn file_refs(schema: &FormDocument, answers: &Value) -> Vec<AnswerFileReference> {
    validate_answers(
        schema,
        Some(answers),
        &ValidateAnswersOptions {
            mode: AnswerValidationMode::Draft,
            time_zone: "UTC",
        },
    )
    .files
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct MediaRow {
    uuid: String,
    object_key: String,
    content_type: String,
    size_bytes: Option<i32>,
    is_confirmed: bool,
}

async fn media_rows(
    state: &AppState,
    submission_id: i32,
    refs: &[AnswerFileReference],
) -> sqlx::Result<HashMap<String, MediaRow>> {
    let uuids: Vec<String> = refs
        .iter()
        .map(|r| r.file_uuid.clone())
        .filter(|u| is_uuid_shape(u))
        .collect();
    let rows: Vec<MediaRow> = sqlx::query_as(
        r#"SELECT uuid::text AS uuid, "objectKey" AS object_key, "contentType" AS content_type,
                  "sizeBytes" AS size_bytes, "isConfirmed" AS is_confirmed
             FROM "storeVisitFormFile"
            WHERE "submissionId" = $1 AND uuid = ANY($2::uuid[])"#,
    )
    .bind(submission_id)
    .bind(&uuids)
    .fetch_all(&state.db)
    .await?;
    Ok(rows.into_iter().map(|r| (r.uuid.clone(), r)).collect())
}

/// Bagian murni `enrichMediaValues`: tulis bentuk baca ke setiap jalur rujukan.
/// `url_of(fileUuid)` hanya dipanggil untuk baris terkonfirmasi.
pub fn apply_enrichment<F>(
    answers: &Value,
    refs: &[AnswerFileReference],
    rows: &HashMap<String, (String, Option<i32>, bool)>,
    mut url_of: F,
) -> ApiResult<Value>
where
    F: FnMut(&str) -> ApiResult<String>,
{
    let mut enriched = answers.clone();
    for r in refs {
        // Kunci peta = uuid kanonis (huruf kecil) dari DB; rujukan dicocokkan apa adanya.
        let Some((content_type, size_bytes, confirmed)) = rows.get(&r.file_uuid) else {
            continue;
        };
        let url = if *confirmed {
            Value::String(url_of(&r.file_uuid)?)
        } else {
            Value::Null
        };
        set_value_at_field_path(
            &mut enriched,
            &r.field_path,
            json!({
                "fileUuid": r.file_uuid,
                "contentType": content_type,
                "sizeBytes": size_bytes,
                "url": url,
            }),
        );
    }
    Ok(enriched)
}

/// `enrichMediaValues(schema, answers, submission, audience)` (store-visit-form-files.service.ts:445):
/// setiap `{ fileUuid }` → `{ fileUuid, contentType, sizeBytes, url }`. Pemanggil WAJIB sudah
/// mengecek cakupan submission. Mobile tanpa `FILE_URL_SECRET` → 500.
pub async fn enrich_media_values(
    state: &AppState,
    schema: &FormDocument,
    answers: &Value,
    submission_id: i32,
    submission_uuid: &str,
    audience: Audience,
) -> ApiResult<Value> {
    let refs = file_refs(schema, answers);
    if refs.is_empty() {
        return Ok(answers.clone());
    }
    let rows = media_rows(state, submission_id, &refs).await?;
    let slim: HashMap<String, (String, Option<i32>, bool)> = rows
        .into_iter()
        .map(|(k, r)| (k, (r.content_type, r.size_bytes, r.is_confirmed)))
        .collect();
    apply_enrichment(answers, &refs, &slim, |file_uuid| match audience {
        Audience::Admin => Ok(admin_form_file_path(submission_uuid, file_uuid)),
        Audience::Mobile => signed_mobile_form_file_url(state, submission_uuid, file_uuid)
            .map_err(|_| ApiError::internal(anyhow::anyhow!("FILE_URL_SECRET is not set"))),
    })
}

// ── Fase 0.3 — byte media untuk ekspor PDF ───────────────────────────────────

/// Kandidat unduhan media PDF.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddableFile {
    pub uuid: String,
    pub object_key: String,
    pub content_type: String,
}

/// `selectEmbeddableFiles` (store-visit-form-files.service.ts:612): hanya gambar milik node
/// `signature`/`photo`, tanpa duplikat, maksimal [`PDF_MEDIA_MAX_FILES`].
pub fn select_embeddable_files(
    schema: &FormDocument,
    refs: &[AnswerFileReference],
    by_uuid: &HashMap<String, EmbeddableFile>,
) -> Vec<EmbeddableFile> {
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    for r in refs {
        if selected.len() >= PDF_MEDIA_MAX_FILES {
            break;
        }
        if seen.contains(r.file_uuid.as_str()) {
            continue;
        }
        let Some(row) = by_uuid.get(&r.file_uuid) else {
            continue;
        };
        if !row.content_type.starts_with("image/") {
            continue;
        }
        let Some(node) = find_node_by_field_path(schema, &r.field_path) else {
            continue;
        };
        if !matches!(node.ty, FormNodeType::Signature | FormNodeType::Photo) {
            continue;
        }
        seen.insert(r.file_uuid.as_str());
        selected.push(row.clone());
    }
    selected
}

/// `shrinkForPdf` (store-visit-form-files.service.ts:662) — pengganti Jimp: hanya gambar
/// di atas 300 KB DAN lebih lebar dari 1400 px yang di-resize ke lebar 1400 (tinggi
/// proporsional); PNG tetap PNG, selain itu JPEG. Gagal decode → byte asli.
pub fn shrink_for_pdf(file_uuid: &str, buffer: Vec<u8>, content_type: &str) -> PdfMedia {
    let keep = |buffer: Vec<u8>| PdfMedia {
        buffer,
        content_type: content_type.to_owned(),
    };
    if buffer.len() <= PDF_MEDIA_SHRINK_THRESHOLD_BYTES {
        return keep(buffer);
    }
    let image = match image::load_from_memory(&buffer) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!("failed to downscale media {file_uuid} for PDF export: {e}");
            return keep(buffer);
        }
    };
    if image.width() <= PDF_MEDIA_MAX_WIDTH {
        return keep(buffer);
    }
    let height = ((f64::from(image.height()) * f64::from(PDF_MEDIA_MAX_WIDTH)
        / f64::from(image.width()))
    .round() as u32)
        .max(1);
    let resized = image.resize_exact(
        PDF_MEDIA_MAX_WIDTH,
        height,
        image::imageops::FilterType::Triangle,
    );
    let png = content_type == "image/png";
    let mut out = Cursor::new(Vec::new());
    let encoded = if png {
        resized.write_to(&mut out, image::ImageFormat::Png)
    } else {
        let rgb = image::DynamicImage::ImageRgb8(resized.to_rgb8());
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 100);
        enc.encode_image(&rgb)
    };
    match encoded {
        Ok(()) => PdfMedia {
            buffer: out.into_inner(),
            content_type: if png { "image/png" } else { "image/jpeg" }.to_owned(),
        },
        Err(e) => {
            tracing::warn!("failed to downscale media {file_uuid} for PDF export: {e}");
            keep(buffer)
        }
    }
}

async fn shrink_blocking(file_uuid: String, buffer: Vec<u8>, content_type: String) -> PdfMedia {
    let fallback = (buffer.len() <= PDF_MEDIA_SHRINK_THRESHOLD_BYTES).then(|| PdfMedia {
        buffer: buffer.clone(),
        content_type: content_type.clone(),
    });
    if let Some(m) = fallback {
        return m;
    }
    let ct = content_type.clone();
    match tokio::task::spawn_blocking(move || shrink_for_pdf(&file_uuid, buffer, &ct)).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("downscale task failed: {e}");
            PdfMedia {
                buffer: Vec::new(),
                content_type,
            }
        }
    }
}

/// Hasil `loadMediaForPdf`.
#[derive(Debug, Clone, Default)]
pub struct PdfMediaLoad {
    /// `answers` dengan nilai media diperkaya `{ fileUuid, contentType, sizeBytes }`.
    pub answers: Value,
    /// fileUuid → byte siap tanam (yang gagal/terpotong plafon tidak ada).
    pub media: HashMap<String, PdfMedia>,
}

/// `loadMediaForPdf(schema, answers, submissionId)` (store-visit-form-files.service.ts:514).
/// `answers` WAJIB sudah dipangkas `prune_hidden_values`. Tidak pernah gagal karena unduhan.
pub async fn load_media_for_pdf(
    state: &AppState,
    schema: &FormDocument,
    answers: &Value,
    submission_id: i32,
) -> ApiResult<PdfMediaLoad> {
    let mut media = HashMap::new();
    let refs = file_refs(schema, answers);
    if refs.is_empty() {
        return Ok(PdfMediaLoad {
            answers: answers.clone(),
            media,
        });
    }
    let rows = media_rows(state, submission_id, &refs).await?;

    let mut enriched = answers.clone();
    for r in &refs {
        let Some(row) = rows.get(&r.file_uuid) else {
            continue;
        };
        set_value_at_field_path(
            &mut enriched,
            &r.field_path,
            json!({
                "fileUuid": row.uuid,
                "contentType": row.content_type,
                "sizeBytes": row.size_bytes,
            }),
        );
    }

    let by_uuid: HashMap<String, EmbeddableFile> = rows
        .iter()
        .map(|(k, r)| {
            (
                k.clone(),
                EmbeddableFile {
                    uuid: r.uuid.clone(),
                    object_key: r.object_key.clone(),
                    content_type: r.content_type.clone(),
                },
            )
        })
        .collect();
    let queue = select_embeddable_files(schema, &refs, &by_uuid);
    let mut budget = PDF_MEDIA_MAX_TOTAL_BYTES;

    'batches: for batch in queue.chunks(PDF_MEDIA_BATCH_SIZE) {
        if budget <= 0 {
            break;
        }
        let settled = futures::future::join_all(
            batch
                .iter()
                .map(|f| mz_files::download_buffer(state, &f.object_key)),
        )
        .await;
        for (file, result) in batch.iter().zip(settled) {
            let bytes = match result {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("failed to download media {} for PDF export: {e}", file.uuid);
                    continue;
                }
            };
            if budget <= 0 {
                break 'batches;
            }
            let shrunk = shrink_blocking(file.uuid.clone(), bytes, file.content_type.clone()).await;
            budget -= shrunk.buffer.len() as i64;
            media.insert(file.uuid.clone(), shrunk);
        }
    }

    Ok(PdfMediaLoad {
        answers: enriched,
        media,
    })
}

// ── §6.5 sumber ke-1 — cron pembersih ────────────────────────────────────────

/// Hasil penyapuan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupResult {
    pub deleted_rows: u64,
    pub deleted_objects: u64,
}

fn cutoff(older_than_hours: i64) -> chrono::NaiveDateTime {
    (Utc::now() - chrono::Duration::hours(older_than_hours)).naive_utc()
}

/// Inti dua penyapu: hapus baris (bersyarat `isConfirmed = false`) LEBIH DULU, baru objeknya.
async fn sweep_unconfirmed(
    state: &AppState,
    table: &str,
    older_than_hours: i64,
) -> anyhow::Result<CleanupResult> {
    let stale: Vec<(i32, String)> = sqlx::query_as(&format!(
        r#"SELECT id, "objectKey" FROM "{table}" WHERE "isConfirmed" = false AND "createdAt" < $1"#
    ))
    .bind(cutoff(older_than_hours))
    .fetch_all(&state.db)
    .await?;
    let mut out = CleanupResult::default();
    for (id, key) in stale {
        let count = sqlx::query(&format!(
            r#"DELETE FROM "{table}" WHERE id = $1 AND "isConfirmed" = false"#
        ))
        .bind(id)
        .execute(&state.db)
        .await?
        .rows_affected();
        if count == 0 {
            // Dikonfirmasi setelah potret diambil — bukan milik sapuan ini.
            continue;
        }
        out.deleted_rows += count;
        match mz_files::delete_file(state, &key).await {
            Ok(_) => out.deleted_objects += 1,
            Err(e) => tracing::warn!("failed to delete bucket object {key}: {e}"),
        }
    }
    Ok(out)
}

/// `cleanupUnconfirmedFiles(olderThanHours = 24)` (store-visit-form-files.service.ts:710).
pub async fn cleanup_unconfirmed_files(
    state: &AppState,
    older_than_hours: i64,
) -> anyhow::Result<CleanupResult> {
    let r = sweep_unconfirmed(state, "storeVisitFormFile", older_than_hours).await?;
    if r.deleted_rows == 0 && r.deleted_objects == 0 {
        // Node: `stale.length === 0` → kembali tanpa log; baris yang semuanya diambil
        // alih confirm tetap dicatat (perbedaan log saja).
        return Ok(r);
    }
    tracing::info!(
        "cleaned up {} unconfirmed store-visit-form file(s), {} bucket object(s) removed",
        r.deleted_rows,
        r.deleted_objects
    );
    Ok(r)
}

/// `cleanupUnconfirmedTemplateAssets(olderThanHours = 24)` (store-visit-form-files.service.ts:977).
pub async fn cleanup_unconfirmed_template_assets(
    state: &AppState,
    older_than_hours: i64,
) -> anyhow::Result<CleanupResult> {
    let r = sweep_unconfirmed(state, "storeVisitFormTemplateAsset", older_than_hours).await?;
    if r.deleted_rows > 0 {
        tracing::info!(
            "cleaned up {} unconfirmed store-visit-form template asset(s), {} bucket object(s) removed",
            r.deleted_rows,
            r.deleted_objects
        );
    }
    Ok(r)
}

// ── Aset kop/kaki PDF ────────────────────────────────────────────────────────

/// Hasil `registerTemplateAsset` (JSON `{assetUuid, uploadUrl, contentType}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisteredAsset {
    pub asset_uuid: String,
    pub upload_url: String,
    pub content_type: String,
}

/// `registerTemplateAsset(params)` (store-visit-form-files.service.ts:759): INSERT tunggal di
/// luar transaksi, lalu tanda tangani URL tulis.
pub async fn register_template_asset(
    state: &AppState,
    template_id: i32,
    template_uuid: &str,
    company_id: i32,
    user_id: i32,
    content_type: &str,
    size_bytes: i64,
) -> ApiResult<RegisteredAsset> {
    let uuid = uuid::Uuid::new_v4().to_string();
    let object_key = format!(
        "{}{uuid}.{}",
        template_asset_prefix(company_id, template_uuid),
        extension_for(content_type)
    );
    sqlx::query(
        r#"INSERT INTO "storeVisitFormTemplateAsset"
             (uuid, "templateId", "companyId", "objectKey", "contentType", "sizeBytes",
              "isConfirmed", "createdById")
           VALUES ($1::uuid, $2, $3, $4, $5, $6, false, $7)"#,
    )
    .bind(&uuid)
    .bind(template_id)
    .bind(company_id)
    .bind(&object_key)
    .bind(content_type)
    .bind(i32::try_from(size_bytes).unwrap_or(i32::MAX))
    .bind(user_id)
    .execute(&state.db)
    .await?;
    let upload_url = sign_upload_url(state, &object_key, content_type).await?;
    Ok(RegisteredAsset {
        asset_uuid: uuid,
        upload_url,
        content_type: content_type.to_owned(),
    })
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct AssetRow {
    id: i32,
    uuid: String,
    object_key: String,
    content_type: String,
    size_bytes: Option<i32>,
    is_confirmed: bool,
}

const ASSET_COLUMNS: &str = r#"id, uuid::text AS uuid, "objectKey" AS object_key,
    "contentType" AS content_type, "sizeBytes" AS size_bytes, "isConfirmed" AS is_confirmed"#;

/// `toAssetResponse` — tanpa objectKey.
pub fn to_asset_response(
    uuid: &str,
    content_type: &str,
    size_bytes: Option<i32>,
    is_confirmed: bool,
) -> Value {
    json!({
        "assetUuid": uuid,
        "contentType": content_type,
        "sizeBytes": size_bytes,
        "isConfirmed": is_confirmed,
    })
}

fn asset_response(a: &AssetRow) -> Value {
    to_asset_response(&a.uuid, &a.content_type, a.size_bytes, a.is_confirmed)
}

/// `confirmTemplateAsset(templateId, assetUuid)` (store-visit-form-files.service.ts:795).
pub async fn confirm_template_asset(
    state: &AppState,
    template_id: i32,
    asset_uuid: &str,
) -> ApiResult<Value> {
    if !is_uuid_shape(asset_uuid) {
        return Err(asset_not_found());
    }
    let asset: Option<AssetRow> = sqlx::query_as(&format!(
        r#"SELECT {ASSET_COLUMNS} FROM "storeVisitFormTemplateAsset"
            WHERE uuid = $1::uuid AND "templateId" = $2 LIMIT 1"#
    ))
    .bind(asset_uuid)
    .bind(template_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(asset) = asset else {
        return Err(asset_not_found());
    };
    if asset.is_confirmed {
        return Ok(asset_response(&asset));
    }
    let metadata = mz_files::get_object_metadata(state, &asset.object_key).await?;
    if !metadata.exists {
        return Err(unprocessable(
            "FORM_ASSET_NOT_UPLOADED",
            "No uploaded object was found for this asset",
        ));
    }
    if let Some(size) = metadata.size_bytes
        && size as i64 > MAX_LAYOUT_ASSET_BYTES
    {
        return Err(unprocessable_details(
            "FORM_ASSET_TOO_LARGE",
            format!("Uploaded image exceeds the {MAX_LAYOUT_ASSET_BYTES} byte limit"),
            json!({ "maxSizeBytes": MAX_LAYOUT_ASSET_BYTES }),
        ));
    }
    let size_bytes = metadata.size_bytes.map(size_i32).or(asset.size_bytes);
    let updated = sqlx::query(
        r#"UPDATE "storeVisitFormTemplateAsset" SET "isConfirmed" = true, "sizeBytes" = $2
            WHERE id = $1 AND "isConfirmed" = false"#,
    )
    .bind(asset.id)
    .bind(size_bytes)
    .execute(&state.db)
    .await?
    .rows_affected();
    if updated == 0 {
        let current: Option<AssetRow> = sqlx::query_as(&format!(
            r#"SELECT {ASSET_COLUMNS} FROM "storeVisitFormTemplateAsset" WHERE id = $1"#
        ))
        .bind(asset.id)
        .fetch_optional(&state.db)
        .await?;
        return current
            .map(|c| asset_response(&c))
            .ok_or_else(asset_not_found);
    }
    Ok(asset_response(&AssetRow {
        is_confirmed: true,
        size_bytes,
        ..asset
    }))
}

/// `resolveTemplateAsset` → `(key, contentType)`; 404 `FORM_ASSET_NOT_FOUND`.
async fn resolve_template_asset(
    state: &AppState,
    template_id: i32,
    asset_uuid: &str,
) -> ApiResult<(String, String)> {
    if !is_uuid_shape(asset_uuid) {
        return Err(asset_not_found());
    }
    let row: Option<(String, String, i32, String)> = sqlx::query_as(
        r#"SELECT a."objectKey", a."contentType", a."companyId", t.uuid::text
             FROM "storeVisitFormTemplateAsset" a
             JOIN "storeVisitFormTemplate" t ON t.id = a."templateId"
            WHERE a.uuid = $1::uuid AND a."templateId" = $2 AND a."isConfirmed" = true
            LIMIT 1"#,
    )
    .bind(asset_uuid)
    .bind(template_id)
    .fetch_optional(&state.db)
    .await?;
    let bucket = bucket(state);
    row.and_then(|(key, ct, company, tpl)| {
        stored_template_asset_key(bucket.as_deref(), &key, company, &tpl).map(|k| (k, ct))
    })
    .ok_or_else(asset_not_found)
}

/// `streamTemplateAsset(templateId, assetUuid, headers, res)` (store-visit-form-files.service.ts:844).
/// Pemanggil sudah meresolusi `templateId` dari company-nya sendiri.
pub async fn stream_template_asset(
    state: &AppState,
    template_id: i32,
    asset_uuid: &str,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    let (key, _) = resolve_template_asset(state, template_id, asset_uuid).await?;
    mz_files::stream_object(state, &key, Some(headers), StreamObjectOptions::default()).await
}

/// `templateAssetUrl(templateId, assetUuid)` (store-visit-form-files.service.ts:855) — signed URL
/// GCS baca (`{url, expiresAt}`). Route memasang `Cache-Control: SIGNED_URL_RESPONSE_CACHE_CONTROL`.
pub async fn template_asset_url(
    state: &AppState,
    template_id: i32,
    asset_uuid: &str,
) -> ApiResult<SignedReadUrl> {
    let (key, content_type) = resolve_template_asset(state, template_id, asset_uuid).await?;
    signed_url(state, &key, &content_type).await
}

async fn signed_url(state: &AppState, key: &str, content_type: &str) -> ApiResult<SignedReadUrl> {
    Ok(mz_files::get_signed_object_url(
        state,
        key,
        &SignedObjectOpts {
            content_type: Some(Some(content_type)),
            filename: None,
        },
        Utc::now().timestamp_millis(),
    )
    .await?)
}

/// `findMissingTemplateAssets(client, templateId, assetUuids)` (store-visit-form-files.service.ts:892):
/// `assetUuid` yang dirujuk tapi BUKAN aset terkonfirmasi template ini (urutan kemunculan
/// pertama). `conn` boleh transaksi (publish) atau koneksi pool (dry-run validate).
pub async fn find_missing_template_assets(
    conn: &mut PgConnection,
    template_id: i32,
    asset_uuids: &[String],
) -> sqlx::Result<Vec<String>> {
    let mut wanted: Vec<String> = Vec::new();
    for u in asset_uuids {
        if !wanted.contains(u) {
            wanted.push(u.clone());
        }
    }
    if wanted.is_empty() {
        return Ok(wanted);
    }
    let valid: Vec<&String> = wanted.iter().filter(|u| is_uuid_shape(u)).collect();
    let found: Vec<String> = sqlx::query_scalar(
        r#"SELECT uuid::text FROM "storeVisitFormTemplateAsset"
            WHERE "templateId" = $1 AND "isConfirmed" = true AND uuid = ANY($2::uuid[])"#,
    )
    .bind(template_id)
    .bind(&valid)
    .fetch_all(&mut *conn)
    .await?;
    let found: HashSet<String> = found.into_iter().map(|u| u.to_lowercase()).collect();
    Ok(wanted.into_iter().filter(|u| !found.contains(u)).collect())
}

/// `loadLayoutMediaForPdf({ schema, companyId, templateId })` (store-visit-form-files.service.ts:913):
/// byte gambar kop/kaki, dikunci `assetUuid` (huruf kecil). Gagal-lunak.
pub async fn load_layout_media_for_pdf(
    state: &AppState,
    schema: &Value,
    template_id: i32,
) -> ApiResult<HashMap<String, PdfMedia>> {
    let mut media = HashMap::new();
    let refs = collect_layout_asset_references(schema);
    if refs.is_empty() {
        return Ok(media);
    }
    let mut uuids: Vec<String> = Vec::new();
    for r in refs {
        if !uuids.contains(&r.asset_uuid) {
            uuids.push(r.asset_uuid);
        }
    }
    let rows: Vec<(String, String, String, i32, String)> = sqlx::query_as(
        r#"SELECT a.uuid::text, a."objectKey", a."contentType", a."companyId", t.uuid::text
             FROM "storeVisitFormTemplateAsset" a
             JOIN "storeVisitFormTemplate" t ON t.id = a."templateId"
            WHERE a."templateId" = $1 AND a."isConfirmed" = true AND a.uuid = ANY($2::uuid[])"#,
    )
    .bind(template_id)
    .bind(&uuids)
    .fetch_all(&state.db)
    .await?;
    let bucket = bucket(state);
    let jobs: Vec<(String, String, String)> = rows
        .into_iter()
        .filter_map(|(uuid, key, ct, company, tpl)| {
            stored_template_asset_key(bucket.as_deref(), &key, company, &tpl)
                .map(|k| (uuid.to_lowercase(), k, ct))
        })
        .collect();
    let settled = futures::future::join_all(
        jobs.iter()
            .map(|(_, key, _)| mz_files::download_buffer(state, key)),
    )
    .await;
    for ((id, _, ct), result) in jobs.into_iter().zip(settled) {
        match result {
            Ok(bytes) => {
                let shrunk = shrink_blocking(id.clone(), bytes, ct).await;
                media.insert(id, shrunk);
            }
            Err(e) => tracing::warn!("failed to download layout image {id} for PDF export: {e}"),
        }
    }
    Ok(media)
}

// ── private_file_access.md F2 — proxy byte media ─────────────────────────────

/// Cakupan submission untuk `resolveFile`.
#[derive(Debug, Clone, Copy)]
struct SubmissionScope<'a> {
    uuid: &'a str,
    company_id: Option<i32>,
}

/// `resolveFile` (store-visit-form-files.service.ts:1114): milik submission itu, terkonfirmasi,
/// DAN dirujuk `answers`-nya. 404 `FORM_FILE_NOT_FOUND` untuk semua kegagalan.
async fn resolve_file(
    state: &AppState,
    scope: SubmissionScope<'_>,
    file_uuid: &str,
) -> ApiResult<(String, String)> {
    if !is_uuid_shape(file_uuid) || !is_uuid_shape(scope.uuid) {
        return Err(file_not_found());
    }
    #[derive(sqlx::FromRow)]
    struct Row {
        uuid: String,
        object_key: String,
        content_type: String,
        company_id: i32,
        visit_uuid: String,
        answers: Value,
        template_version_id: i32,
    }
    let row: Option<Row> = sqlx::query_as(
        r#"SELECT f.uuid::text AS uuid, f."objectKey" AS object_key, f."contentType" AS content_type,
                  s."companyId" AS company_id, s."visitUuid"::text AS visit_uuid, s.answers,
                  s."templateVersionId" AS template_version_id
             FROM "storeVisitFormFile" f
             JOIN "storeVisitFormSubmission" s ON s.id = f."submissionId"
            WHERE f.uuid = $1::uuid AND f."isConfirmed" = true AND s.uuid = $2::uuid
              AND ($3::int IS NULL OR s."companyId" = $3)
            LIMIT 1"#,
    )
    .bind(file_uuid)
    .bind(scope.uuid)
    .bind(scope.company_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else {
        return Err(file_not_found());
    };
    let key = stored_form_file_key(
        bucket(state).as_deref(),
        &row.object_key,
        row.company_id,
        &row.visit_uuid,
    )
    .ok_or_else(file_not_found)?;
    let schema: Option<Value> =
        sqlx::query_scalar(r#"SELECT schema FROM "storeVisitFormTemplateVersion" WHERE id = $1"#)
            .bind(row.template_version_id)
            .fetch_optional(&state.db)
            .await?;
    let schema = FormDocument::from_value(&schema.ok_or_else(file_not_found)?);
    let referenced = file_refs(&schema, &row.answers)
        .iter()
        .any(|r| r.file_uuid == row.uuid);
    if !referenced {
        return Err(file_not_found());
    }
    Ok((key, row.content_type))
}

/// `streamCompanyFile(companyId, submissionUuid, fileUuid, headers, res)`
/// (store-visit-form-files.service.ts:1025) — admin ber-JWT.
pub async fn stream_company_file(
    state: &AppState,
    company_id: i32,
    submission_uuid: &str,
    file_uuid: &str,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    let scope = SubmissionScope {
        uuid: submission_uuid,
        company_id: Some(company_id),
    };
    let (key, _) = resolve_file(state, scope, file_uuid).await?;
    mz_files::stream_object(state, &key, Some(headers), StreamObjectOptions::default()).await
}

/// `companyFileUrl(companyId, submissionUuid, fileUuid)` (store-visit-form-files.service.ts:1101).
pub async fn company_file_url(
    state: &AppState,
    company_id: i32,
    submission_uuid: &str,
    file_uuid: &str,
) -> ApiResult<SignedReadUrl> {
    let scope = SubmissionScope {
        uuid: submission_uuid,
        company_id: Some(company_id),
    };
    let (key, content_type) = resolve_file(state, scope, file_uuid).await?;
    signed_url(state, &key, &content_type).await
}

/// `streamSignedFile(submissionUuid, fileUuid, exp, sig, headers, res)`
/// (store-visit-form-files.service.ts:1046) — mobile tanpa JWT. Header `Referrer-Policy` &
/// `X-Robots-Tag` ikut di SEMUA respons (termasuk 404).
pub async fn stream_signed_file(
    state: &AppState,
    submission_uuid: &str,
    file_uuid: &str,
    exp: Option<&str>,
    sig: Option<&str>,
    headers: &HeaderMap,
) -> Response {
    let result = async {
        let path = mobile_form_file_path(submission_uuid, file_uuid);
        if !mz_files::verify_file_path_signature(state, &path, exp, sig) {
            return Err(file_not_found());
        }
        let scope = SubmissionScope {
            uuid: submission_uuid,
            company_id: None,
        };
        let (key, _) = resolve_file(state, scope, file_uuid).await?;
        mz_files::stream_object(
            state,
            &key,
            Some(headers),
            StreamObjectOptions {
                cache_control: Some(mz_files::SIGNED_FILE_CACHE_CONTROL.to_owned()),
                ..Default::default()
            },
        )
        .await
    }
    .await;
    let mut resp = match result {
        Ok(r) => r,
        Err(e) => e.into_response(),
    };
    mz_files::set_unauthenticated_file_headers(resp.headers_mut());
    resp
}

// ── Route `mobile-store-visit-form-file.controller.ts` ───────────────────────

/// `main.ts` `mobileFileLimiter`: 300/menit per IP, `rl:mobile-signed-file`.
const MOBILE_FILE_LIMIT: RateRule = RateRule {
    key_prefix: "rl:mobile-signed-file",
    max: Some(300),
    window_ms: 60_000,
    key: RateKey::Ip,
    pass_on_store_error: false,
};

/// `MobileStoreVisitFormFileController` — `@PermissionExempt('no-user')`, tanpa JWT.
const SIGNED_FILE: RouteSpec = RouteSpec::public()
    .exempt(Exempt::NoUser)
    .rate_limits(&[MOBILE_FILE_LIMIT]);

/// Node: src/store-visit-forms/mobile-store-visit-form-file.controller.ts:64 `submissionFile`.
async fn h_submission_file(
    State(state): State<AppState>,
    Path((uuid, file_uuid)): Path<(String, String)>,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let uuid = uuid_pipe(uuid)?;
    let file_uuid = uuid_pipe(file_uuid)?;
    let q = mz_core::qs::parse(uri.query().unwrap_or_default());
    let exp = mz_core::qs::get(&q, "exp").and_then(|v| v.as_str());
    let sig = mz_core::qs::get(&q, "sig").and_then(|v| v.as_str());
    Ok(stream_signed_file(&state, &uuid, &file_uuid, exp, sig, &headers).await)
}

/// Route proxy file mobile bertanda tangan.
pub fn router(state: &AppState) -> Router<AppState> {
    use mz_core::gate::guard;
    Router::new().route(
        "/v1/mobile/store-visit-forms/submissions/{uuid}/files/{file_uuid}",
        guard(state, SIGNED_FILE, get(h_submission_file)),
    )
}

// ── Penanda jalan cron (dipakai `crate::cron`) ───────────────────────────────

/// `StoreVisitFormsCron.running` — hanya dalam satu proses.
pub(crate) static SWEEP_RUNNING: AtomicBool = AtomicBool::new(false);

/// Ambil penanda; `false` = jalan sebelumnya masih berlangsung.
pub(crate) fn try_begin_sweep() -> bool {
    SWEEP_RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

pub(crate) fn end_sweep() {
    SWEEP_RUNNING.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID_A: &str = "8f1c2d3e-4a5b-4c6d-8e9f-0a1b2c3d4e5f";
    const UUID_B: &str = "3ab70000-1111-4222-8333-444455556666";

    fn schema() -> FormDocument {
        FormDocument::from_value(&json!({
            "schemaVersion": 1,
            "fields": [
                {"key": "sec", "type": "section", "label": "Persetujuan", "fields": [
                    {"key": "diserahkan", "type": "group", "label": "Diserahkan", "fields": [
                        {"key": "ttd", "type": "signature", "label": "Tanda tangan"}
                    ]}
                ]},
                {"key": "unit_list", "type": "repeater", "label": "Unit",
                 "validation": {"minRows": 0, "maxRows": 10}, "fields": [
                    {"key": "unit", "type": "text", "label": "Unit"},
                    {"key": "foto", "type": "photo", "label": "Foto",
                     "validation": {"maxSizeBytes": 500000, "accept": ["image/jpeg"]}}
                ]},
                {"key": "lampiran", "type": "file", "label": "Lampiran"}
            ]
        }))
    }

    fn req(field_path: &str, content_type: &str, size_bytes: i64) -> UploadRequest {
        UploadRequest {
            field_path: field_path.into(),
            content_type: content_type.into(),
            size_bytes,
        }
    }

    fn body(e: ApiError) -> (u16, Value) {
        let status = e.status().as_u16();
        match e {
            ApiError::Body { body, .. } => (status, body),
            other => panic!("unexpected {other:?}"),
        }
    }

    fn plan(r: &UploadRequest) -> ApiResult<RegisteredUpload> {
        plan_upload(&schema(), 10, "visit-uuid", r, UUID_A)
    }

    #[test]
    fn register_writes_server_object_key() {
        let p = plan(&req("diserahkan.ttd", "image/png", 84_213)).unwrap();
        assert_eq!(
            p.object_key,
            format!("store-visit-forms/10/visit-uuid/{UUID_A}.png")
        );
        assert_eq!(p.content_type, "image/png");
        // Menembus section & repeater ber-nomor baris.
        assert!(plan(&req("unit_list.2.foto", "image/jpeg", 1000)).is_ok());
        let p = plan(&req("lampiran", "application/pdf", 1)).unwrap();
        assert!(p.object_key.ends_with(".pdf"));
        let p = plan(&req("lampiran", "text/csv", 1)).unwrap();
        assert!(p.object_key.ends_with(".bin"));
    }

    #[test]
    fn register_rejects_non_media_paths() {
        for path in [
            "unit_list.0.unit",
            "diserahkan",
            "sec",
            "tidak_ada",
            "unit_list.foto",
        ] {
            let (status, b) = body(plan(&req(path, "image/png", 1)).unwrap_err());
            assert_eq!(status, 422);
            assert_eq!(
                b,
                json!({"code": "FORM_FIELD_NOT_MEDIA",
                       "message": format!("\"{path}\" is not a signature, photo, or file field in this form")})
            );
        }
    }

    #[test]
    fn register_enforces_types_and_sizes() {
        let (_, b) = body(plan(&req("diserahkan.ttd", "image/jpeg", 1)).unwrap_err());
        assert_eq!(b["message"], "A signature must be uploaded as image/png");
        let (_, b) = body(plan(&req("unit_list.0.foto", "application/pdf", 1)).unwrap_err());
        assert_eq!(b["message"], "A photo must be an image");
        let (_, b) = body(plan(&req("unit_list.0.foto", "image/png", 1)).unwrap_err());
        assert_eq!(
            b,
            json!({"code": "FORM_FILE_TYPE_REJECTED",
                   "message": "\"image/png\" is not accepted by this field",
                   "details": {"accept": ["image/jpeg"]}})
        );
        let (status, b) = body(plan(&req("unit_list.0.foto", "image/jpeg", 500_001)).unwrap_err());
        assert_eq!(status, 422);
        assert_eq!(
            b,
            json!({"code": "FORM_FILE_TOO_LARGE",
                   "message": "File exceeds the 500000 byte limit for this field",
                   "details": {"maxSizeBytes": 500000}})
        );
        assert!(plan(&req("unit_list.0.foto", "image/jpeg", 500_000)).is_ok());
        // Plafon keras 2 MB untuk node tanpa maxSizeBytes.
        let (_, b) = body(plan(&req("lampiran", "application/pdf", 2_097_153)).unwrap_err());
        assert_eq!(b["details"]["maxSizeBytes"], 2_097_152);
    }

    #[test]
    fn accept_wildcard_subtype() {
        let doc = FormDocument::from_value(&json!({"schemaVersion": 1, "fields": [
            {"key": "f", "type": "file", "validation": {"accept": ["image/*", "application/pdf"]}}
        ]}));
        let node = &doc.fields[0];
        assert!(assert_content_type_allowed(node, "image/webp").is_ok());
        assert!(assert_content_type_allowed(node, "application/pdf").is_ok());
        assert!(assert_content_type_allowed(node, "application/zip").is_err());
    }

    #[test]
    fn response_shapes_hide_object_key() {
        let v = to_file_response(UUID_A, Some("diserahkan.ttd"), "image/png", Some(5), true);
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            format!(
                r#"{{"fileUuid":"{UUID_A}","fieldPath":"diserahkan.ttd","contentType":"image/png","sizeBytes":5,"isConfirmed":true}}"#
            )
        );
        let v = to_asset_response(UUID_A, "image/png", None, false);
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            format!(
                r#"{{"assetUuid":"{UUID_A}","contentType":"image/png","sizeBytes":null,"isConfirmed":false}}"#
            )
        );
    }

    fn stored() -> Vec<StoredFile> {
        vec![
            StoredFile {
                id: 1,
                uuid: UUID_A.into(),
                object_key: "key-a".into(),
                is_confirmed: true,
                field_path: Some("diserahkan.ttd".into()),
            },
            StoredFile {
                id: 2,
                uuid: UUID_B.into(),
                object_key: "key-b".into(),
                is_confirmed: true,
                field_path: Some("unit_list.0.foto".into()),
            },
        ]
    }

    fn used(uuid: &str, path: &str) -> AnswerFileReference {
        AnswerFileReference {
            file_uuid: uuid.into(),
            field_path: path.into(),
        }
    }

    #[test]
    fn reconcile_rejects_foreign_or_unconfirmed() {
        let (status, b) = body(plan_reconcile(&[], &[used(UUID_A, "diserahkan.ttd")]).unwrap_err());
        assert_eq!(status, 409);
        assert_eq!(
            b,
            json!({"code": "FORM_FILE_NOT_CONFIRMED",
                   "message": "Every file referenced in the answers must belong to this submission and be confirmed",
                   "details": {"files": [{"fieldPath": "diserahkan.ttd", "fileUuid": UUID_A}]}})
        );
        let mut s = stored();
        s[0].is_confirmed = false;
        assert!(plan_reconcile(&s[..1], &[used(UUID_A, "diserahkan.ttd")]).is_err());
    }

    #[test]
    fn reconcile_returns_orphans_and_rewrites_paths() {
        let p = plan_reconcile(&stored(), &[used(UUID_A, "diserahkan.ttd")]).unwrap();
        assert_eq!(p.orphan_ids, vec![2]);
        assert_eq!(p.orphan_keys, vec!["key-b".to_owned()]);
        assert!(p.path_updates.is_empty());

        // Nomor baris bergeser: file tetap dipakai, fieldPath diisi ulang.
        let mut s = stored();
        s[1].field_path = Some("unit_list.1.foto".into());
        let p = plan_reconcile(&s[1..], &[used(UUID_B, "unit_list.0.foto")]).unwrap();
        assert!(p.orphan_keys.is_empty());
        assert_eq!(p.path_updates, vec![(2, "unit_list.0.foto".to_owned())]);

        let p = plan_reconcile(&stored(), &[]).unwrap();
        assert_eq!(p.orphan_keys, vec!["key-a".to_owned(), "key-b".to_owned()]);
    }

    #[test]
    fn enrichment_admin_and_unconfirmed() {
        let answers = json!({
            "diserahkan": {"ttd": {"fileUuid": UUID_A}},
            "unit_list": [{"unit": "A"}, {"unit": "B", "foto": {"fileUuid": UUID_B}}]
        });
        let refs = vec![
            used(UUID_A, "diserahkan.ttd"),
            used(UUID_B, "unit_list.1.foto"),
        ];
        let mut rows = HashMap::new();
        rows.insert(
            UUID_A.to_owned(),
            ("image/png".to_owned(), Some(84_213), true),
        );
        rows.insert(
            UUID_B.to_owned(),
            ("image/jpeg".to_owned(), Some(5000), false),
        );
        let sub = "5f0e8a51-1c2b-4d3e-9f40-6a7b8c9d0e1f";
        let out =
            apply_enrichment(&answers, &refs, &rows, |f| Ok(admin_form_file_path(sub, f))).unwrap();
        assert_eq!(
            out["diserahkan"]["ttd"],
            json!({"fileUuid": UUID_A, "contentType": "image/png", "sizeBytes": 84213,
                   "url": format!("/v1/admin/store-visit-forms/submissions/{sub}/files/{UUID_A}")})
        );
        assert_eq!(out["unit_list"][1]["foto"]["url"], Value::Null);
        // Milik pemanggil tidak berubah; baris yang tidak ditemukan dibiarkan.
        assert_eq!(answers["diserahkan"]["ttd"], json!({"fileUuid": UUID_A}));
        let out =
            apply_enrichment(&answers, &refs, &HashMap::new(), |_| Ok(String::new())).unwrap();
        assert_eq!(out, answers);
    }

    #[test]
    fn embeddable_selection() {
        let s = schema();
        let mut by = HashMap::new();
        let row = |u: &str, ct: &str| EmbeddableFile {
            uuid: u.into(),
            object_key: format!("k-{u}"),
            content_type: ct.into(),
        };
        by.insert(UUID_A.to_owned(), row(UUID_A, "image/png"));
        by.insert(UUID_B.to_owned(), row(UUID_B, "image/jpeg"));
        by.insert("c".to_owned(), row("c", "application/pdf"));
        by.insert("d".to_owned(), row("d", "image/png"));
        let refs = vec![
            used(UUID_A, "diserahkan.ttd"),
            used(UUID_B, "unit_list.0.foto"),
            used(UUID_B, "unit_list.1.foto"), // duplikat → satu unduhan
            used("c", "lampiran"),            // bukan gambar
            used("d", "lampiran"),            // node file tidak pernah ditanam
            used("zz", "diserahkan.ttd"),     // baris tidak ada
        ];
        let out = select_embeddable_files(&s, &refs, &by);
        let ids: Vec<&str> = out.iter().map(|f| f.uuid.as_str()).collect();
        assert_eq!(ids, vec![UUID_A, UUID_B]);

        // Plafon 40 berkas.
        let mut many = HashMap::new();
        let refs: Vec<AnswerFileReference> = (0..50)
            .map(|i| {
                let u = format!("u{i}");
                many.insert(u.clone(), row(&u, "image/jpeg"));
                used(&u, &format!("unit_list.{i}.foto"))
            })
            .collect();
        assert_eq!(
            select_embeddable_files(&s, &refs, &many).len(),
            PDF_MEDIA_MAX_FILES
        );
    }

    fn noise_image(w: u32, h: u32) -> image::RgbImage {
        let mut seed: u32 = 12345;
        image::RgbImage::from_fn(w, h, |_, _| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let b = seed.to_le_bytes();
            image::Rgb([b[1], b[2], b[3]])
        })
    }

    fn encode(img: image::RgbImage, fmt: image::ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, fmt)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn shrink_small_and_narrow_pass_through() {
        let small = vec![1u8; 1000];
        assert_eq!(
            shrink_for_pdf("x", small.clone(), "image/png").buffer,
            small
        );
        // Besar tapi tidak bisa di-decode (HEIC dsb.) → byte asli.
        let junk = vec![7u8; PDF_MEDIA_SHRINK_THRESHOLD_BYTES + 1];
        let m = shrink_for_pdf("x", junk.clone(), "image/heic");
        assert_eq!((m.buffer, m.content_type.as_str()), (junk, "image/heic"));
        // Besar tapi lebarnya sudah <= 1400 → apa adanya.
        let narrow = encode(noise_image(1000, 200), image::ImageFormat::Png);
        assert!(narrow.len() > PDF_MEDIA_SHRINK_THRESHOLD_BYTES);
        assert_eq!(
            shrink_for_pdf("x", narrow.clone(), "image/png").buffer,
            narrow
        );
    }

    #[test]
    fn shrink_wide_png_stays_png_jpeg_otherwise() {
        let wide = encode(noise_image(2000, 200), image::ImageFormat::Png);
        assert!(wide.len() > PDF_MEDIA_SHRINK_THRESHOLD_BYTES);
        let m = shrink_for_pdf("x", wide.clone(), "image/png");
        assert_eq!(m.content_type, "image/png");
        let img = image::load_from_memory(&m.buffer).unwrap();
        assert_eq!((img.width(), img.height()), (1400, 140));

        let m = shrink_for_pdf("x", wide, "image/webp");
        assert_eq!(m.content_type, "image/jpeg");
        assert_eq!(
            image::guess_format(&m.buffer).unwrap(),
            image::ImageFormat::Jpeg
        );
    }

    #[test]
    fn stored_keys_must_live_in_own_folder() {
        let tpl = "AB70000-0000";
        assert_eq!(
            template_asset_prefix(10, "ABC-Def"),
            "store-visit-forms/10/templates/abc-def/"
        );
        let key = format!(
            "store-visit-forms/10/templates/{}/x.png",
            tpl.to_lowercase()
        );
        assert_eq!(
            stored_template_asset_key(Some("b"), &key, 10, tpl).as_deref(),
            Some(key.as_str())
        );
        assert!(stored_template_asset_key(Some("b"), &key, 11, tpl).is_none());
        assert!(stored_template_asset_key(Some("b"), "", 10, tpl).is_none());

        let key = "store-visit-forms/10/v-1/f.png";
        assert_eq!(
            stored_form_file_key(Some("b"), key, 10, "v-1").as_deref(),
            Some(key)
        );
        assert!(stored_form_file_key(Some("b"), key, 10, "v-2").is_none());
        assert!(
            stored_form_file_key(Some("b"), "store-visit-forms/9/v-1/f.png", 10, "v-1").is_none()
        );
        assert_eq!(
            form_file_object_key(10, "v", "u", "image/jpeg"),
            "store-visit-forms/10/v/u.jpg"
        );
    }
}
