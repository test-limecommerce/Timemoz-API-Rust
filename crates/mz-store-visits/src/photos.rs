//! Port `src/store-visits/store-visit-photos.service.ts` (§6.5) — unggah foto tiga langkah
//! langsung ke bucket, serialisasi foto (mobile: URL proxy bertanda tangan; admin: signed URL
//! GCS), status foto wajib untuk check-out, penyapu foto yatim (cron), proxy byte foto.
//! Beserta DTO `CreatePhotoUploadUrlDto` dan `ConfirmPhotoDto`.

use std::collections::HashMap;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use mz_core::jsfmt::num;
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, ApiResult, AppState};
use mz_files::{SignedObjectOpts, SignedReadUrl, StreamObjectOptions};
use mz_sales_geo::dto::{as_number, check_number, present};
use serde_json::{Map, Value, json};

use crate::enums::{PHOTO_CONTENT_TYPES, StoreVisitPhotoKind, is_enum_message};
use crate::photo_url::{mobile_visit_photo_file_path, signed_mobile_visit_photo_url};

/// §6.5 — 2 MB.
pub const MAX_PHOTO_BYTES: i64 = 2 * 1024 * 1024;
/// Umur upload URL (§6.5, §9.5).
pub const UPLOAD_URL_TTL_SECONDS: i64 = 900;
/// Baris belum terkonfirmasi lebih tua dari ini disapu (§6.5).
pub const UNCONFIRMED_MAX_AGE_HOURS: i64 = 24;
/// Kandidat per putaran `cleanup_unconfirmed_photos` (keyset `id`).
pub const UNCONFIRMED_SWEEP_BATCH_SIZE: i64 = 500;
/// Foto wajib sebelum check-out (§5.3.4); `result` selalu opsional.
pub const REQUIRED_PHOTO_KINDS: [StoreVisitPhotoKind; 2] =
    [StoreVisitPhotoKind::store, StoreVisitPhotoKind::selfie];

fn extension_for(content_type: &str) -> Option<&'static str> {
    match content_type {
        "image/jpeg" => Some("jpg"),
        "image/png" => Some("png"),
        _ => None,
    }
}

// ───────────────────────── DTO ─────────────────────────

/// `CreatePhotoUploadUrlDto`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreatePhotoUploadUrl {
    pub kind: StoreVisitPhotoKind,
    /// Sengaja bukan `@IsEnum` — diperiksa service (422 `PHOTO_CONTENT_TYPE_INVALID`).
    pub content_type: String,
    /// Batas 2 MB ditegakkan service (422 `PHOTO_TOO_LARGE`).
    pub size_bytes: i64,
}

/// `@IsInt() @Min(min)` (pesan: Min, IsInt) atas nilai JSON mentah.
fn check_int_min(errs: &mut Vec<String>, field: &str, v: Option<&Value>, min: i64) -> Option<i64> {
    let n = as_number(v);
    let before = errs.len();
    #[allow(clippy::cast_precision_loss)]
    if n.is_none_or(|x| x < min as f64) {
        errs.push(format!("{field} must not be less than {min}"));
    }
    let int = n.filter(|x| x.fract() == 0.0);
    if int.is_none() {
        errs.push(format!("{field} must be an integer number"));
    }
    #[allow(clippy::cast_possible_truncation)]
    int.filter(|_| errs.len() == before).map(|x| x as i64)
}

/// Validasi `CreatePhotoUploadUrlDto` (urutan properti: kind, contentType, sizeBytes).
pub fn parse_create_upload_url(m: &Map<String, Value>) -> ApiResult<CreatePhotoUploadUrl> {
    let mut errs = Vec::new();
    let kind = match m.get("kind") {
        Some(Value::String(s)) => s.parse::<StoreVisitPhotoKind>().ok(),
        _ => None,
    };
    if kind.is_none() {
        errs.push(is_enum_message(
            "kind",
            &crate::enum_values!(StoreVisitPhotoKind),
        ));
    }
    let content_type =
        mz_sales_geo::dto::check_string(&mut errs, "contentType", m.get("contentType"));
    let size = check_int_min(&mut errs, "sizeBytes", m.get("sizeBytes"), 1);
    match (kind, content_type, size) {
        (Some(kind), Some(content_type), Some(size_bytes)) if errs.is_empty() => {
            Ok(CreatePhotoUploadUrl {
                kind,
                content_type,
                size_bytes,
            })
        }
        _ => Err(ApiError::Validation(errs)),
    }
}

/// `ConfirmPhotoDto`.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfirmPhoto {
    pub taken_at: i64,
    pub lat: Option<f64>,
    pub lng: Option<f64>,
}

/// Validasi `ConfirmPhotoDto` (takenAt `@IsInt @Min(0)`; lat/lng `@IsOptional @IsNumber @Min @Max`).
pub fn parse_confirm(m: &Map<String, Value>) -> ApiResult<ConfirmPhoto> {
    let mut errs = Vec::new();
    let taken_at = check_int_min(&mut errs, "takenAt", m.get("takenAt"), 0);
    let mut coord = |key: &str, lim: f64| -> Option<f64> {
        let v = present(m, key)?;
        check_number(&mut errs, key, as_number(Some(v)), Some(-lim), Some(lim))
    };
    let lat = coord("lat", 90.0);
    let lng = coord("lng", 180.0);
    match taken_at {
        Some(taken_at) if errs.is_empty() => Ok(ConfirmPhoto { taken_at, lat, lng }),
        _ => Err(ApiError::Validation(errs)),
    }
}

// ───────────────────────── baris & bentuk respons ─────────────────────────

/// Kolom foto yang dibaca serializer (`PhotoRow` + `contentType`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PhotoRow {
    pub id: i32,
    #[sqlx(rename = "visitId")]
    pub visit_id: i32,
    pub uuid: String,
    pub kind: String,
    #[sqlx(rename = "objectKey")]
    pub object_key: String,
    #[sqlx(rename = "contentType")]
    pub content_type: String,
    #[sqlx(rename = "sizeBytes")]
    pub size_bytes: Option<i32>,
    #[sqlx(rename = "isConfirmed")]
    pub is_confirmed: bool,
    #[sqlx(rename = "takenAt")]
    pub taken_at: Option<JsDate>,
    pub lat: Option<f64>,
    pub lng: Option<f64>,
}

/// `SELECT` kolom [`PhotoRow`] dari `"storeVisitPhoto" p` (tambahkan `WHERE`).
pub const PHOTO_SELECT: &str = r#"SELECT p.id, p."visitId", p.uuid::text AS uuid, p.kind::text AS kind,
       p."objectKey", p."contentType", p."sizeBytes", p."isConfirmed", p."takenAt", p.lat, p.lng
  FROM "storeVisitPhoto" p"#;

/// Semua foto (terkonfirmasi atau tidak) milik kunjungan-kunjungan ini, urut `id`.
pub async fn photos_of_visits(db: &sqlx::PgPool, visit_ids: &[i32]) -> ApiResult<Vec<PhotoRow>> {
    if visit_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as(&format!(
        r#"{PHOTO_SELECT} WHERE p."visitId" = ANY($1) ORDER BY p.id"#
    ))
    .bind(visit_ids)
    .fetch_all(db)
    .await?)
}

fn photo_response(
    uuid: &str,
    kind: &str,
    taken_at: Option<JsDate>,
    lat: Option<f64>,
    lng: Option<f64>,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("id".into(), Value::from(uuid));
    m.insert("kind".into(), Value::from(kind));
    m.insert(
        "takenAt".into(),
        taken_at.map_or(Value::Null, |t| Value::from(t.timestamp_millis())),
    );
    m.insert("lat".into(), lat.map_or(Value::Null, num));
    m.insert("lng".into(), lng.map_or(Value::Null, num));
    m
}

/// `toPhotoResponse`: `{ id, kind, takenAt (ms|null), lat, lng }`.
pub fn to_photo_response(p: &PhotoRow) -> Value {
    Value::Object(photo_response(&p.uuid, &p.kind, p.taken_at, p.lat, p.lng))
}

/// `summarizePhotos` (§6.6, §7.4): `{ photosCount, photoKinds }` dari foto TERKONFIRMASI
/// (kind unik, urutan kemunculan). Tanpa signed URL.
pub fn summarize_photos<'a>(
    photos: impl IntoIterator<Item = (&'a str, bool)>,
) -> (usize, Vec<String>) {
    let mut count = 0;
    let mut kinds: Vec<String> = Vec::new();
    for (kind, confirmed) in photos {
        if !confirmed {
            continue;
        }
        count += 1;
        if !kinds.iter().any(|k| k == kind) {
            kinds.push(kind.to_owned());
        }
    }
    (count, kinds)
}

/// `serializePhotos(visitUuid, photos)` (mobile): foto terkonfirmasi + `url` proxy bertanda
/// tangan (absolut). Tanpa `FILE_URL_SECRET` → 500.
pub fn serialize_photos(
    state: &AppState,
    visit_uuid: &str,
    photos: &[PhotoRow],
) -> ApiResult<Vec<Value>> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    photos
        .iter()
        .filter(|p| p.is_confirmed)
        .map(|p| {
            let mut m = photo_response(&p.uuid, &p.kind, p.taken_at, p.lat, p.lng);
            m.insert(
                "url".into(),
                Value::from(signed_mobile_visit_photo_url(
                    state, visit_uuid, &p.uuid, now_ms,
                )?),
            );
            Ok(Value::Object(m))
        })
        .collect()
}

/// `serializeAdminPhotos(visit, photos)` (dashboard): foto terkonfirmasi + `url` signed GCS
/// (15–25 mnt); key di luar folder kunjungan atau gagal tanda tangan → `url: null`.
pub async fn serialize_admin_photos(
    state: &AppState,
    visit_uuid: &str,
    company_id: i32,
    photos: &[PhotoRow],
) -> Vec<Value> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let futs = photos
        .iter()
        .filter(|p| p.is_confirmed)
        .map(|p| async move {
            let mut m = photo_response(&p.uuid, &p.kind, p.taken_at, p.lat, p.lng);
            let url = sign_admin_photo_url(state, visit_uuid, company_id, p, now_ms).await;
            m.insert("url".into(), url.map_or(Value::Null, Value::from));
            Value::Object(m)
        });
    futures::future::join_all(futs).await
}

async fn sign_admin_photo_url(
    state: &AppState,
    visit_uuid: &str,
    company_id: i32,
    photo: &PhotoRow,
    now_ms: i64,
) -> Option<String> {
    let key = stored_photo_key(state, &photo.object_key, visit_uuid, company_id)?;
    let opts = SignedObjectOpts {
        content_type: Some(Some(photo.content_type.as_str())),
        filename: None,
    };
    match mz_files::get_signed_object_url(state, &key, &opts, now_ms).await {
        Ok(s) => Some(s.url),
        Err(e) => {
            tracing::warn!("failed to sign read URL for {key}: {e}");
            None
        }
    }
}

/// `storedPhotoKey`: key tersimpan, hanya bila sah DAN di folder `store-visits/<companyId>/<visitUuid>/`.
pub fn stored_photo_key(
    state: &AppState,
    object_key: &str,
    visit_uuid: &str,
    company_id: i32,
) -> Option<String> {
    let key = mz_files::object_key_of(state, object_key).ok()?;
    key_in_visit_folder(&key, visit_uuid, company_id).then_some(key)
}

/// Bagian murni [`stored_photo_key`].
pub fn key_in_visit_folder(key: &str, visit_uuid: &str, company_id: i32) -> bool {
    key.starts_with(&format!("store-visits/{company_id}/{visit_uuid}/"))
}

/// Ringkasan foto terkonfirmasi untuk check-out (§6.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhotoStatus {
    pub photos_count: usize,
    pub photo_kinds: Vec<String>,
    pub missing_required_kinds: Vec<StoreVisitPhotoKind>,
}

/// `confirmedPhotoStatus(visitDbId)`: satu query.
pub async fn confirmed_photo_status<'e>(
    exec: impl sqlx::PgExecutor<'e>,
    visit_id: i32,
) -> ApiResult<PhotoStatus> {
    let kinds: Vec<String> = sqlx::query_scalar(
        r#"SELECT kind::text FROM "storeVisitPhoto" WHERE "visitId" = $1 AND "isConfirmed" = true ORDER BY id"#,
    )
    .bind(visit_id)
    .fetch_all(exec)
    .await?;
    Ok(photo_status_of(&kinds))
}

/// Bagian murni [`confirmed_photo_status`].
pub fn photo_status_of(confirmed_kinds: &[String]) -> PhotoStatus {
    let (count, present) = summarize_photos(confirmed_kinds.iter().map(|k| (k.as_str(), true)));
    PhotoStatus {
        photos_count: count,
        missing_required_kinds: REQUIRED_PHOTO_KINDS
            .iter()
            .copied()
            .filter(|k| !present.iter().any(|p| p == k.as_str()))
            .collect(),
        photo_kinds: present,
    }
}

/// `confirmedPhotoCounts(visitDbIds)`: jumlah foto terkonfirmasi per kunjungan (absen = 0).
pub async fn confirmed_photo_counts(
    db: &sqlx::PgPool,
    visit_ids: &[i32],
) -> ApiResult<HashMap<i32, i64>> {
    if visit_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(i32, i64)> = sqlx::query_as(
        r#"SELECT "visitId", COUNT(*) FROM "storeVisitPhoto"
            WHERE "visitId" = ANY($1) AND "isConfirmed" = true GROUP BY "visitId""#,
    )
    .bind(visit_ids)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().collect())
}

/// `missingRequiredPhotoKinds(visitDbId)`.
pub async fn missing_required_photo_kinds(
    db: &sqlx::PgPool,
    visit_id: i32,
) -> ApiResult<Vec<StoreVisitPhotoKind>> {
    Ok(confirmed_photo_status(db, visit_id)
        .await?
        .missing_required_kinds)
}

// ───────────────────────── unggah (langkah 1 & 3) ─────────────────────────

/// Kunjungan milik caller yang masih aktif.
#[derive(Debug, Clone, sqlx::FromRow)]
struct OwnVisit {
    id: i32,
    uuid: String,
    #[sqlx(rename = "companyId")]
    company_id: i32,
    state: String,
}

fn visit_not_found() -> ApiError {
    ApiError::coded(StatusCode::NOT_FOUND, "VISIT_NOT_FOUND", "Visit not found")
}

fn photo_not_found() -> ApiError {
    ApiError::coded(
        StatusCode::NOT_FOUND,
        "PHOTO_NOT_FOUND",
        "Photo not found for this visit",
    )
}

fn too_large(message: &str) -> ApiError {
    ApiError::body(
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({ "code": "PHOTO_TOO_LARGE", "message": message, "details": { "maxSizeBytes": MAX_PHOTO_BYTES } }),
    )
}

/// `findActiveOwnVisit(userId, visitId)`: 404 `VISIT_NOT_FOUND`, 409 `VISIT_NOT_ACTIVE`.
async fn find_active_own_visit(
    db: &sqlx::PgPool,
    user_id: i32,
    visit_uuid: &str,
) -> ApiResult<OwnVisit> {
    let visit: Option<OwnVisit> = sqlx::query_as(
        r#"SELECT id, uuid::text AS uuid, "companyId", state::text AS state
             FROM "storeVisit" WHERE uuid = $1::uuid AND "userId" = $2 LIMIT 1"#,
    )
    .bind(visit_uuid)
    .bind(user_id)
    .fetch_optional(db)
    .await?;
    let visit = visit.ok_or_else(visit_not_found)?;
    if visit.state != "active" {
        return Err(ApiError::coded(
            StatusCode::CONFLICT,
            "VISIT_NOT_ACTIVE",
            "This visit is no longer active",
        ));
    }
    Ok(visit)
}

async fn delete_object_quietly(state: &AppState, key: &str) -> bool {
    match mz_files::delete_file(state, key).await {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("failed to delete bucket object {key}: {e}");
            false
        }
    }
}

/// Langkah 1 `createUploadUrl(userId, visitId, dto)` → `{ photoId, objectKey, uploadUrl,
/// expiresInSeconds, maxSizeBytes }`. Kind yang sama menggantikan pendaftaran sebelumnya.
pub async fn create_upload_url(
    state: &AppState,
    user_id: i32,
    visit_uuid: &str,
    dto: &CreatePhotoUploadUrl,
) -> ApiResult<Value> {
    let visit = find_active_own_visit(&state.db, user_id, visit_uuid).await?;
    let Some(extension) = extension_for(&dto.content_type) else {
        return Err(ApiError::coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "PHOTO_CONTENT_TYPE_INVALID",
            "Only image/jpeg and image/png are accepted",
        ));
    };
    debug_assert!(PHOTO_CONTENT_TYPES.contains(&dto.content_type.as_str()));
    if dto.size_bytes > MAX_PHOTO_BYTES {
        return Err(too_large("Photo exceeds the 2 MB limit"));
    }
    let photo_uuid = uuid::Uuid::new_v4().to_string();
    let kind = dto.kind.as_str();
    let object_key = format!(
        "store-visits/{}/{}/{photo_uuid}-{kind}.{extension}",
        visit.company_id, visit.uuid
    );

    let mut tx = state.db.begin().await?;
    let created_id: i32 = sqlx::query_scalar(
        r#"INSERT INTO "storeVisitPhoto" (uuid, "visitId", kind, "objectKey", "sizeBytes", "contentType", "isConfirmed")
           VALUES ($1::uuid, $2, $3::"StoreVisitPhotoKind", $4, $5, $6, false) RETURNING id"#,
    )
    .bind(&photo_uuid)
    .bind(visit.id)
    .bind(kind)
    .bind(&object_key)
    .bind(i32::try_from(dto.size_bytes).unwrap_or(i32::MAX))
    .bind(&dto.content_type)
    .fetch_one(&mut *tx)
    .await?;
    let losers: Vec<(i32, String)> = sqlx::query_as(
        r#"SELECT id, "objectKey" FROM "storeVisitPhoto"
            WHERE "visitId" = $1 AND kind = $2::"StoreVisitPhotoKind" AND id <> $3"#,
    )
    .bind(visit.id)
    .bind(kind)
    .bind(created_id)
    .fetch_all(&mut *tx)
    .await?;
    if !losers.is_empty() {
        let ids: Vec<i32> = losers.iter().map(|(id, _)| *id).collect();
        sqlx::query(r#"DELETE FROM "storeVisitPhoto" WHERE id = ANY($1)"#)
            .bind(&ids)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    // Panggilan bucket di luar transaksi (§14.1 poin 9).
    for (_, key) in &losers {
        delete_object_quietly(state, key).await;
    }
    let signed = mz_files::get_signed_file_url(
        state,
        &object_key,
        UPLOAD_URL_TTL_SECONDS,
        &dto.content_type,
    )
    .await?;
    Ok(json!({
        "photoId": photo_uuid,
        "objectKey": object_key,
        "uploadUrl": signed.url,
        "expiresInSeconds": UPLOAD_URL_TTL_SECONDS,
        "maxSizeBytes": MAX_PHOTO_BYTES,
    }))
}

/// Langkah 3 `confirmPhoto(userId, visitId, photoId, dto)`: baca balik object dari bucket, lalu
/// konfirmasi bersyarat (`isConfirmed = false`). Idempoten.
pub async fn confirm_photo(
    state: &AppState,
    user_id: i32,
    visit_uuid: &str,
    photo_uuid: &str,
    dto: &ConfirmPhoto,
) -> ApiResult<Value> {
    let visit = find_active_own_visit(&state.db, user_id, visit_uuid).await?;
    let photo: Option<PhotoRow> = sqlx::query_as(&format!(
        r#"{PHOTO_SELECT} WHERE p.uuid = $1::uuid AND p."visitId" = $2 LIMIT 1"#
    ))
    .bind(photo_uuid)
    .bind(visit.id)
    .fetch_optional(&state.db)
    .await?;
    let photo = photo.ok_or_else(photo_not_found)?;
    if photo.is_confirmed {
        return Ok(to_photo_response(&photo));
    }
    let meta = mz_files::get_object_metadata(state, &photo.object_key).await?;
    if !meta.exists {
        return Err(ApiError::coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "PHOTO_NOT_UPLOADED",
            "No uploaded file was found for this photo",
        ));
    }
    if meta
        .size_bytes
        .is_some_and(|s| i64::try_from(s).unwrap_or(i64::MAX) > MAX_PHOTO_BYTES)
    {
        return Err(too_large("Uploaded file exceeds the 2 MB limit"));
    }
    let taken_at = JsDate::from_millis(dto.taken_at)
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("Invalid Date")))?;
    let size = meta
        .size_bytes
        .and_then(|s| i32::try_from(s).ok())
        .or(photo.size_bytes);
    let n = sqlx::query(
        r#"UPDATE "storeVisitPhoto" SET "isConfirmed" = true, "takenAt" = $2, lat = $3, lng = $4, "sizeBytes" = $5
            WHERE id = $1 AND "isConfirmed" = false"#,
    )
    .bind(photo.id)
    .bind(taken_at)
    .bind(dto.lat)
    .bind(dto.lng)
    .bind(size)
    .execute(&state.db)
    .await?
    .rows_affected();
    if n == 0 {
        let current: Option<PhotoRow> = sqlx::query_as(&format!(
            r#"{PHOTO_SELECT} WHERE p.id = $1 AND p."isConfirmed" = true LIMIT 1"#
        ))
        .bind(photo.id)
        .fetch_optional(&state.db)
        .await?;
        return current
            .map(|c| to_photo_response(&c))
            .ok_or_else(photo_not_found);
    }
    Ok(Value::Object(photo_response(
        &photo.uuid,
        &photo.kind,
        Some(taken_at),
        dto.lat,
        dto.lng,
    )))
}

// ───────────────────────── penyapu (cron) ─────────────────────────

/// `cleanupUnconfirmedPhotos(olderThanHours)` → `(deletedRows, deletedObjects)`. Baris diklaim
/// lebih dulu (DELETE bersyarat), object bucket dihapus hanya bila klaim menghapus tepat 1 baris.
pub async fn cleanup_unconfirmed_photos(
    state: &AppState,
    older_than_hours: i64,
) -> ApiResult<(u64, u64)> {
    let cutoff = now_naive() - chrono::Duration::hours(older_than_hours);
    let (mut rows, mut objects) = (0u64, 0u64);
    let mut last_id = 0i32;
    loop {
        let batch: Vec<(i32, String)> = sqlx::query_as(
            r#"SELECT id, "objectKey" FROM "storeVisitPhoto"
                WHERE "isConfirmed" = false AND "createdAt" < $1 AND id > $2
                ORDER BY id ASC LIMIT $3"#,
        )
        .bind(cutoff)
        .bind(last_id)
        .bind(UNCONFIRMED_SWEEP_BATCH_SIZE)
        .fetch_all(&state.db)
        .await?;
        let Some((tail, _)) = batch.last() else {
            break;
        };
        let tail = *tail;
        for (id, key) in &batch {
            let n = sqlx::query(
                r#"DELETE FROM "storeVisitPhoto" WHERE id = $1 AND "isConfirmed" = false AND "createdAt" < $2"#,
            )
            .bind(id)
            .bind(cutoff)
            .execute(&state.db)
            .await?
            .rows_affected();
            if n != 1 {
                continue;
            }
            rows += 1;
            if delete_object_quietly(state, key).await {
                objects += 1;
            }
        }
        last_id = tail;
        if i64::try_from(batch.len()).unwrap_or(i64::MAX) < UNCONFIRMED_SWEEP_BATCH_SIZE {
            break;
        }
    }
    if rows > 0 {
        tracing::info!(
            "cleaned up {rows} unconfirmed store-visit photo(s), {objects} bucket object(s) removed"
        );
    }
    Ok((rows, objects))
}

// ───────────────────────── proxy byte foto (private_file_access.md F2) ─────────────────────────

/// 404 `VISIT_PHOTO_NOT_FOUND`.
pub fn photo_file_not_found() -> ApiError {
    ApiError::coded(
        StatusCode::NOT_FOUND,
        "VISIT_PHOTO_NOT_FOUND",
        "Photo not found",
    )
}

/// `resolvePhoto(visitWhere, photoUuid)`: foto TERKONFIRMASI pada kunjungan `visit_uuid`
/// (opsional company); key harus di folder kunjungannya. 404 seragam.
async fn resolve_photo(
    state: &AppState,
    visit_uuid: &str,
    company_id: Option<i32>,
    photo_uuid: &str,
) -> ApiResult<(String, String)> {
    let row: Option<(String, String, String, i32)> = sqlx::query_as(
        r#"SELECT p."objectKey", p."contentType", v.uuid::text, v."companyId"
             FROM "storeVisitPhoto" p JOIN "storeVisit" v ON v.id = p."visitId"
            WHERE p.uuid = $1::uuid AND p."isConfirmed" = true AND v.uuid = $2::uuid
              AND ($3::int IS NULL OR v."companyId" = $3)
            LIMIT 1"#,
    )
    .bind(photo_uuid)
    .bind(visit_uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((object_key, content_type, v_uuid, v_company)) = row else {
        return Err(photo_file_not_found());
    };
    let key = stored_photo_key(state, &object_key, &v_uuid, v_company)
        .ok_or_else(photo_file_not_found)?;
    Ok((key, content_type))
}

/// `streamCompanyPhoto` (admin, JWT).
pub async fn stream_company_photo(
    state: &AppState,
    company_id: i32,
    visit_uuid: &str,
    photo_uuid: &str,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    let (key, _) = resolve_photo(state, visit_uuid, Some(company_id), photo_uuid).await?;
    mz_files::stream_object(state, &key, Some(headers), StreamObjectOptions::default()).await
}

/// `companyPhotoUrl` (admin, JWT): signed URL GCS `{ url, expiresAt }`.
pub async fn company_photo_url(
    state: &AppState,
    company_id: i32,
    visit_uuid: &str,
    photo_uuid: &str,
) -> ApiResult<SignedReadUrl> {
    let (key, content_type) =
        resolve_photo(state, visit_uuid, Some(company_id), photo_uuid).await?;
    let opts = SignedObjectOpts {
        content_type: Some(Some(content_type.as_str())),
        filename: None,
    };
    Ok(
        mz_files::get_signed_object_url(state, &key, &opts, chrono::Utc::now().timestamp_millis())
            .await?,
    )
}

/// `streamSignedPhoto` (mobile tanpa JWT): `sig` dicek atas path kanonis, lalu DB tetap dicek.
/// `Referrer-Policy`/`X-Robots-Tag` di SEMUA respons (termasuk 404).
pub async fn stream_signed_photo(
    state: &AppState,
    visit_uuid: &str,
    photo_uuid: &str,
    exp: Option<&str>,
    sig: Option<&str>,
    headers: &HeaderMap,
) -> Response {
    let result = async {
        let path = mobile_visit_photo_file_path(visit_uuid, photo_uuid);
        if !mz_files::verify_file_path_signature(state, &path, exp, sig) {
            return Err(photo_file_not_found());
        }
        let (key, _) = resolve_photo(state, visit_uuid, None, photo_uuid).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn upload_url_dto() {
        let ok = parse_create_upload_url(&obj(
            json!({"kind":"store","contentType":"image/gif","sizeBytes":5}),
        ))
        .unwrap();
        assert_eq!(ok.kind, StoreVisitPhotoKind::store);
        assert_eq!(ok.content_type, "image/gif");
        let Err(ApiError::Validation(e)) =
            parse_create_upload_url(&obj(json!({"kind":"x","sizeBytes":1.5})))
        else {
            panic!()
        };
        assert_eq!(
            e,
            vec![
                "kind must be one of the following values: store, selfie, result",
                "contentType must be a string",
                "sizeBytes must be an integer number"
            ]
        );
        let Err(ApiError::Validation(e)) =
            parse_create_upload_url(&obj(json!({"kind":"store","contentType":"image/png"})))
        else {
            panic!()
        };
        assert_eq!(
            e,
            vec![
                "sizeBytes must not be less than 1",
                "sizeBytes must be an integer number"
            ]
        );
    }

    #[test]
    fn confirm_dto() {
        let ok =
            parse_confirm(&obj(json!({"takenAt": 1_784_589_180_000_i64, "lat": null}))).unwrap();
        assert_eq!(
            ok,
            ConfirmPhoto {
                taken_at: 1_784_589_180_000,
                lat: None,
                lng: None
            }
        );
        let Err(ApiError::Validation(e)) =
            parse_confirm(&obj(json!({"takenAt": "1", "lat": 91, "lng": "x"})))
        else {
            panic!()
        };
        assert_eq!(
            e,
            vec![
                "takenAt must not be less than 0",
                "takenAt must be an integer number",
                "lat must not be greater than 90",
                "lng must not be greater than 180",
                "lng must not be less than -180",
                "lng must be a number conforming to the specified constraints"
            ]
        );
    }

    #[test]
    fn summaries() {
        let (n, kinds) = summarize_photos([
            ("store", true),
            ("selfie", false),
            ("store", true),
            ("result", true),
        ]);
        assert_eq!(n, 3);
        assert_eq!(kinds, vec!["store", "result"]);
        let st = photo_status_of(&["selfie".to_owned()]);
        assert_eq!(st.missing_required_kinds, vec![StoreVisitPhotoKind::store]);
        assert_eq!(
            photo_status_of(&["store".into(), "selfie".into()]).missing_required_kinds,
            vec![]
        );
    }

    #[test]
    fn folder_guard() {
        assert!(key_in_visit_folder("store-visits/1/v/p-store.jpg", "v", 1));
        assert!(!key_in_visit_folder("store-visits/2/v/p-store.jpg", "v", 1));
        assert!(!key_in_visit_folder("store-visits/1/w/p.jpg", "v", 1));
    }

    #[test]
    fn photo_shape() {
        let v = Value::Object(photo_response(
            "u",
            "store",
            JsDate::from_millis(5),
            Some(110.0),
            None,
        ));
        assert_eq!(
            v,
            json!({"id":"u","kind":"store","takenAt":5,"lat":110,"lng":null})
        );
    }
}
