//! Port `src/screenshots/screenshots.service.ts` (tanpa `findAll`, yang tidak punya route, dan
//! tanpa `purgeExpiredScreenshots`, yang hanya dipanggil cron — tidak di-port).

use axum::response::Response;
use http::HeaderMap;
use mz_core::time::{JsDate, now_naive, parse_js_date};
use mz_core::{ApiError, ApiResult, AppState};
use mz_files::{FileRequester, SignedObjectOpts, StreamObjectOptions, load_file_requester};
use serde_json::{Value, json};
use sqlx::{FromRow, PgPool};

use super::access::{
    ScreenshotOwner, can_access_screenshot, is_owner_company_key, object_key_or_null,
    screenshot_image_path, screenshot_upload_key, strip_blur_suffix,
};
use super::dto::{CreateScreenshotDto, UpdateScreenshotDto};
use crate::division::{ViewerScope, can_view_user, resolve_viewer_scope, scope_forbidden};

pub const STATUS_DELETED: i32 = 0;
pub const STATUS_UPLOADING: i32 = 1;
pub const STATUS_FINISHED: i32 = 3;

/// Kolom `screenshot` + rantai pemilik (timeline -> tracking -> user -> company default aktif).
const SHOT_SELECT: &str = r#"
SELECT s.id, s.uuid::text AS uuid, s."fileFullpath", s."isBlur", s."takenAt", s."trackingTimelineId", s."projectId",
       s."createdAt", s."updatedAt", s.status, s."compressedFileKey", s."isCompressed", s."compressedAt",
       p.uuid::text AS "projectUuid",
       tr."userId" AS "ownerUserId", u.uuid::text AS "ownerUserUuid",
       m."companyId" AS "ownerCompanyId", c.uuid::text AS "ownerCompanyUuid",
       c."imageBlurEnabled" AS "imageBlurEnabled", c."imageBlurRoles" AS "imageBlurRoles",
       c."screenshotRetentionDays" AS "retentionDays"
FROM screenshot s
JOIN "trackingTimeline" tt ON tt.id = s."trackingTimelineId"
JOIN tracking tr ON tr.id = tt."trackingId"
JOIN users u ON u.id = tr."userId"
LEFT JOIN project p ON p.id = s."projectId"
LEFT JOIN LATERAL (
    SELECT uc."companyId" FROM "userCompany" uc
    WHERE uc."userId" = u.id AND uc.status = 1
    ORDER BY uc."isDefault" DESC, uc.id ASC LIMIT 1
) m ON TRUE
LEFT JOIN company c ON c.id = m."companyId"
WHERE s.uuid = $1::uuid
"#;

#[derive(Debug, Clone, FromRow)]
pub struct ShotRow {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "fileFullpath")]
    pub file_fullpath: String,
    #[sqlx(rename = "isBlur")]
    pub is_blur: bool,
    #[sqlx(rename = "takenAt")]
    pub taken_at: JsDate,
    #[sqlx(rename = "trackingTimelineId")]
    pub tracking_timeline_id: i32,
    #[sqlx(rename = "projectId")]
    pub project_id: i32,
    #[sqlx(rename = "createdAt")]
    pub created_at: JsDate,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: Option<JsDate>,
    pub status: i32,
    #[sqlx(rename = "compressedFileKey")]
    pub compressed_file_key: Option<String>,
    #[sqlx(rename = "isCompressed")]
    pub is_compressed: bool,
    #[sqlx(rename = "compressedAt")]
    pub compressed_at: Option<JsDate>,
    #[sqlx(rename = "projectUuid")]
    pub project_uuid: Option<String>,
    #[sqlx(rename = "ownerUserId")]
    pub owner_user_id: i32,
    #[sqlx(rename = "ownerUserUuid")]
    pub owner_user_uuid: String,
    #[sqlx(rename = "ownerCompanyId")]
    pub owner_company_id: Option<i32>,
    #[sqlx(rename = "ownerCompanyUuid")]
    pub owner_company_uuid: Option<String>,
    #[sqlx(rename = "imageBlurEnabled")]
    pub image_blur_enabled: Option<bool>,
    #[sqlx(rename = "imageBlurRoles")]
    pub image_blur_roles: Option<Vec<String>>,
    #[sqlx(rename = "retentionDays")]
    pub retention_days: Option<i32>,
}

impl ShotRow {
    /// `screenshotOwnerOf`: `None` bila rantai tidak lengkap.
    pub fn owner(&self) -> Option<ScreenshotOwner> {
        Some(ScreenshotOwner {
            user_id: self.owner_user_id,
            user_uuid: self.owner_user_uuid.clone(),
            company_id: self.owner_company_id?,
            company_uuid: self.owner_company_uuid.clone()?,
        })
    }

    /// Role requester boleh membuka `original` dari screenshot ber-blur (`imageBlurRoles`).
    fn blur_role_allows(&self, role_code: Option<&str>) -> bool {
        self.image_blur_enabled == Some(true)
            && role_code.is_some_and(|r| {
                self.image_blur_roles
                    .as_deref()
                    .is_some_and(|roles| roles.iter().any(|x| x == r))
            })
    }

    /// Semua kolom skalar model `screenshot` (bentuk Prisma tanpa select).
    fn full_json(&self) -> Value {
        json!({
            "id": self.id,
            "uuid": self.uuid,
            "fileFullpath": self.file_fullpath,
            "isBlur": self.is_blur,
            "takenAt": self.taken_at,
            "trackingTimelineId": self.tracking_timeline_id,
            "projectId": self.project_id,
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
            "status": self.status,
            "compressedFileKey": self.compressed_file_key,
            "isCompressed": self.is_compressed,
            "compressedAt": self.compressed_at,
        })
    }
}

async fn fetch_shot(db: &PgPool, uuid: &str) -> sqlx::Result<Option<ShotRow>> {
    sqlx::query_as::<_, ShotRow>(SHOT_SELECT)
        .bind(uuid)
        .fetch_optional(db)
        .await
}

/// Hasil `canViewScreenshot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Allow,
    DenyLeader,
    Deny,
}

/// Aturan Leader di atas `canAccessScreenshot` (`canViewScreenshot`).
fn decide_view(
    owner_user: i32,
    owner_company: i32,
    requester: &FileRequester,
    scope: impl FnOnce() -> Option<ViewerScope>,
) -> View {
    if can_access_screenshot(owner_user, owner_company, requester) {
        return View::Allow;
    }
    if requester.company_id != Some(owner_company) {
        return View::Deny;
    }
    match scope() {
        Some(s @ ViewerScope::Division { .. }) => {
            if can_view_user(&s, requester.user_id, owner_user) {
                View::Allow
            } else {
                View::DenyLeader
            }
        }
        _ => View::Deny,
    }
}

async fn can_view_screenshot(
    db: &PgPool,
    owner_user: i32,
    owner_company: i32,
    requester: &FileRequester,
) -> ApiResult<View> {
    if can_access_screenshot(owner_user, owner_company, requester) {
        return Ok(View::Allow);
    }
    if requester.company_id != Some(owner_company) {
        return Ok(View::Deny);
    }
    let scope = resolve_viewer_scope(db, requester.user_id, owner_company).await?;
    Ok(decide_view(owner_user, owner_company, requester, || {
        Some(scope)
    }))
}

fn not_found() -> ApiError {
    ApiError::not_found("Screenshot not found")
}

/// `GET /screenshots/:uuid` — Node: screenshots.service.ts `findOne`.
pub async fn find_one(state: &AppState, uuid: &str, requester_id: i32) -> ApiResult<Value> {
    let (found, requester) = tokio::try_join!(
        async { fetch_shot(&state.db, uuid).await.map_err(ApiError::from) },
        async {
            load_file_requester(&state.db, requester_id)
                .await
                .map_err(ApiError::from)
        },
    )?;
    let found = found.filter(|s| s.status != STATUS_DELETED);
    let owner = found.as_ref().and_then(ShotRow::owner);
    let access = match (&found, &owner) {
        (Some(_), Some(o)) => {
            can_view_screenshot(&state.db, o.user_id, o.company_id, &requester).await?
        }
        _ => View::Deny,
    };
    if access == View::DenyLeader {
        return Err(scope_forbidden());
    }
    let (Some(found), Some(_), View::Allow) = (found, owner, access) else {
        return Ok(json!({
            "success": false,
            "message": format!("Data screenshot by uuid {uuid} not found"),
            "data": Value::Null,
        }));
    };
    // URL/key GCS tidak keluar; gambar dibuka lewat path proxy.
    Ok(json!({
        "success": true,
        "message": format!("Success get data screenshot by uuid {uuid}"),
        "data": {
            "id": found.id,
            "uuid": found.uuid,
            "isBlur": found.is_blur,
            "takenAt": found.taken_at,
            "trackingTimelineId": found.tracking_timeline_id,
            "projectId": found.project_id,
            "createdAt": found.created_at,
            "updatedAt": found.updated_at,
            "status": found.status,
            "isCompressed": found.is_compressed,
            "compressedAt": found.compressed_at,
            "screenshotUrl": screenshot_image_path(&found.uuid, "display"),
        },
    }))
}

#[derive(Debug, FromRow)]
struct CreateCtx {
    #[sqlx(rename = "userUuid")]
    user_uuid: String,
    #[sqlx(rename = "companyUuid")]
    company_uuid: String,
}

async fn create_inner(
    state: &AppState,
    dto: &CreateScreenshotDto,
    user_id: i32,
) -> Result<Value, anyhow::Error> {
    let str_of = |v: &Option<Value>| match v {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    let project_uuid = str_of(&dto.project_uuid);
    let timeline_uuid = str_of(&dto.tracking_timeline_uuid);

    let ctx: Option<CreateCtx> = sqlx::query_as(
        r#"SELECT u.uuid::text AS "userUuid", c.uuid::text AS "companyUuid"
           FROM users u
           JOIN LATERAL (SELECT uc."companyId" FROM "userCompany" uc
                         WHERE uc."userId" = u.id AND uc.status = 1
                         ORDER BY uc."isDefault" DESC, uc.id ASC LIMIT 1) m ON TRUE
           JOIN company c ON c.id = m."companyId"
           WHERE u.id = $1"#,
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;

    // Timeline wajib milik pemanggil dan project-nya wajib project timeline itu.
    let timeline: Option<(i32, i32)> = sqlx::query_as(
        r#"SELECT tt.id, tt."projectId"
           FROM "trackingTimeline" tt
           JOIN tracking tr ON tr.id = tt."trackingId"
           JOIN project p ON p.id = tt."projectId"
           WHERE tt.uuid = $1::uuid AND tr."userId" = $2 AND p.uuid = $3::uuid
           LIMIT 1"#,
    )
    .bind(&timeline_uuid)
    .bind(user_id)
    .bind(&project_uuid)
    .fetch_optional(&state.db)
    .await?;

    let (Some(ctx), Some((timeline_id, project_id))) = (ctx, timeline) else {
        return Ok(
            json!({"success": false, "message": "Tracking timeline not found", "data": Value::Null}),
        );
    };

    // Key disusun server dan langsung disimpan di baris UPLOADING.
    let screenshot_uuid = uuid::Uuid::new_v4().to_string();
    let key = screenshot_upload_key(
        &ctx.company_uuid,
        &project_uuid,
        &ctx.user_uuid,
        &screenshot_uuid,
    );
    let taken_at = match &dto.taken_at {
        Some(Value::String(s)) => {
            parse_js_date(s).ok_or_else(|| anyhow::anyhow!("invalid takenAt"))?
        }
        _ => anyhow::bail!("invalid takenAt"),
    };
    let is_blur = matches!(dto.is_blur, Some(Value::Bool(true)));

    let id: i32 = sqlx::query_scalar(
        r#"INSERT INTO screenshot (uuid, "trackingTimelineId", "isBlur", "takenAt", "projectId", status, "fileFullpath")
           VALUES ($1::uuid, $2, $3, $4, $5, $6, $7) RETURNING id"#,
    )
    .bind(&screenshot_uuid)
    .bind(timeline_id)
    .bind(is_blur)
    .bind(taken_at)
    .bind(project_id)
    .bind(STATUS_UPLOADING)
    .bind(&key)
    .fetch_one(&state.db)
    .await?;
    let created = fetch_shot(&state.db, &screenshot_uuid)
        .await?
        .ok_or_else(|| anyhow::anyhow!("screenshot {id} vanished after insert"))?;

    let presigned = mz_files::get_signed_file_url(state, &key, 3600, "image/jpeg")
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut data = created.full_json();
    if let Value::Object(map) = &mut data {
        map.insert("presignedUrl".into(), Value::String(presigned.url));
    }
    Ok(json!({"success": true, "message": "register screenshot success", "data": data}))
}

/// `POST /screenshots` — Node: `create`. Semua galat => `{success:false,...}` (bukan 5xx).
pub async fn create(state: &AppState, dto: &CreateScreenshotDto, user_id: i32) -> Value {
    match create_inner(state, dto, user_id).await {
        Ok(v) => v,
        Err(error) => {
            tracing::error!(%error, module = "screenshot-create", "register screenshot failed");
            json!({"success": false, "message": "register screenshot failed", "data": Value::Null})
        }
    }
}

/// Nilai `fileFullpath` yang diharapkan untuk PATCH konfirmasi.
fn expected_key(
    bucket: Option<&str>,
    row: &ShotRow,
    owner: &ScreenshotOwner,
    uuid: &str,
) -> Option<String> {
    object_key_or_null(bucket, Some(&strip_blur_suffix(&row.file_fullpath))).or_else(|| {
        row.project_uuid
            .as_deref()
            .map(|p| screenshot_upload_key(&owner.company_uuid, p, &owner.user_uuid, uuid))
    })
}

/// `PATCH /screenshots/:uuid` — Node: `update` (konfirmasi upload; hanya pemilik).
pub async fn update(
    state: &AppState,
    uuid: &str,
    dto: &UpdateScreenshotDto,
    requester_id: i32,
) -> ApiResult<Value> {
    let row = fetch_shot(&state.db, uuid)
        .await?
        .filter(|s| s.status != STATUS_DELETED);
    let owner = row.as_ref().and_then(ShotRow::owner);
    let (Some(row), Some(owner)) = (row, owner) else {
        return Err(not_found());
    };
    if owner.user_id != requester_id {
        return Err(not_found());
    }

    let bucket = state.config.var("GCP_BUCKET_NAME");
    let expected = expected_key(bucket.as_deref(), &row, &owner, uuid);
    let submitted = object_key_or_null(bucket.as_deref(), dto.file_fullpath_str());
    let Some(expected) = expected.filter(|e| submitted.as_deref() == Some(e.as_str())) else {
        return Err(ApiError::bad_request(
            "fileFullpath does not match the upload key issued for this screenshot",
        ));
    };

    if row.status == STATUS_FINISHED {
        return Ok(
            json!({"success": true, "message": "Screenshot already finished", "data": Value::Null}),
        );
    }

    let outcome = async {
        let is_blur: Option<bool> = match &dto.is_blur {
            None => None,
            Some(Value::Bool(b)) => Some(*b),
            // `isBlur: null` ditolak Prisma (kolom non-null) -> `success: false`.
            Some(_) => anyhow::bail!("isBlur must not be null"),
        };
        let taken_at = match &dto.taken_at {
            Some(Value::String(s)) if !s.is_empty() => {
                Some(parse_js_date(s).ok_or_else(|| anyhow::anyhow!("invalid takenAt"))?)
            }
            _ => None,
        };
        let updated = sqlx::query(
            r#"UPDATE screenshot
               SET "isBlur" = COALESCE($2, "isBlur"), "takenAt" = COALESCE($3, "takenAt"),
                   status = $4, "updatedAt" = $5, "fileFullpath" = $6
               WHERE uuid = $1::uuid AND status = $7"#,
        )
        .bind(uuid)
        .bind(is_blur)
        .bind(taken_at)
        .bind(STATUS_FINISHED)
        .bind(now_naive())
        .bind(&expected)
        .bind(row.status)
        .execute(&state.db)
        .await?;
        if updated.rows_affected() == 0 {
            return Ok(None);
        }
        let data = fetch_shot(&state.db, uuid).await?.map(|s| s.full_json());
        Ok::<_, anyhow::Error>(Some(data))
    }
    .await;

    match outcome {
        Ok(None) => Err(ApiError::conflict(
            "Screenshot changed while confirming upload, please retry",
        )),
        Ok(Some(data)) => {
            Ok(json!({"success": true, "message": "update screenshot success", "data": data}))
        }
        Err(error) => {
            tracing::error!(%error, module = "screenshot-update", "update screenshot failed");
            Ok(
                json!({"success": false, "message": "update screenshot failed", "data": Value::Null}),
            )
        }
    }
}

/// `DELETE /screenshots/:uuid` — Node: `remove`.
pub async fn remove(state: &AppState, uuid: &str, requester_id: i32) -> ApiResult<Value> {
    let (row, requester) = tokio::try_join!(
        async { fetch_shot(&state.db, uuid).await.map_err(ApiError::from) },
        async {
            load_file_requester(&state.db, requester_id)
                .await
                .map_err(ApiError::from)
        },
    )?;
    let owner = row
        .filter(|s| s.status != STATUS_DELETED)
        .and_then(|s| s.owner());
    if !owner.is_some_and(|o| can_access_screenshot(o.user_id, o.company_id, &requester)) {
        return Err(not_found());
    }
    sqlx::query(r#"UPDATE screenshot SET status = $2 WHERE uuid = $1::uuid AND status <> $2"#)
        .bind(uuid)
        .bind(STATUS_DELETED)
        .execute(&state.db)
        .await?;
    Ok(
        json!({"success": true, "message": "delete screenshot success", "data": {"isDeleted": true}}),
    )
}

/// Blur ala Jimp `blur(60)`: JPEG masuk -> JPEG keluar (kualitas 90).
fn blur_jpeg(original: &[u8]) -> anyhow::Result<Vec<u8>> {
    let img = image::load_from_memory(original)?.to_rgb8();
    let blurred = image::imageops::fast_blur(&img, 30.0);
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
        .encode_image(&image::DynamicImage::ImageRgb8(blurred))?;
    Ok(out)
}

async fn blur_inner(
    state: &AppState,
    uuid: &str,
    row: &ShotRow,
    original_key: &str,
) -> anyhow::Result<Value> {
    // 1. Unduh lewat service account (bucket private).
    let original = mz_files::download_buffer(state, original_key)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    // 2. Proses blur (CPU berat -> thread blocking).
    let blurred = tokio::task::spawn_blocking(move || blur_jpeg(&original)).await??;
    // 3. Key baru, tanpa menimpa file lama.
    let extension = original_key.rsplit('.').next().unwrap_or(original_key);
    let base_name = original_key.replacen(&format!(".{extension}"), "", 1);
    let new_key = format!(
        "{base_name}-blurred-{}.{extension}",
        chrono::Utc::now().timestamp_millis()
    );
    // 4. Unggah lewat API storage.
    mz_files::upload_buffer(state, &new_key, blurred, "image/jpeg")
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    // 5. Simpan KEY hasil blur; update bersyarat (TOCTOU).
    let updated = sqlx::query(
        r#"UPDATE screenshot SET "fileFullpath" = $2, "isBlur" = TRUE, "updatedAt" = $3
           WHERE uuid = $1::uuid AND "fileFullpath" = $4"#,
    )
    .bind(uuid)
    .bind(&new_key)
    .bind(now_naive())
    .bind(&row.file_fullpath)
    .execute(&state.db)
    .await?;
    if updated.rows_affected() == 0 {
        let _ = mz_files::delete_file(state, &new_key).await;
        return Ok(
            json!({"success": false, "message": "Screenshot changed while blurring, please retry"}),
        );
    }
    Ok(json!({"success": true, "message": "Image blurred and saved as new file"}))
}

/// `PUT /screenshots/blur/:uuid` — Node: `blur`.
pub async fn blur(state: &AppState, uuid: &str, requester_id: i32) -> ApiResult<Value> {
    let (row, requester) = tokio::try_join!(
        async { fetch_shot(&state.db, uuid).await.map_err(ApiError::from) },
        async {
            load_file_requester(&state.db, requester_id)
                .await
                .map_err(ApiError::from)
        },
    )?;
    let row = row.filter(|s| s.status == STATUS_FINISHED);
    let owner = row.as_ref().and_then(ShotRow::owner);
    let (Some(row), Some(owner)) = (row, owner) else {
        return Ok(json!({"success": false, "message": "Screenshot not found"}));
    };
    if row.file_fullpath.is_empty()
        || !can_access_screenshot(owner.user_id, owner.company_id, &requester)
    {
        return Ok(json!({"success": false, "message": "Screenshot not found"}));
    }

    // URL host/bucket lain ditolak: server tidak pernah mengambil file dari luar bucket sendiri.
    let bucket = state.config.var("GCP_BUCKET_NAME");
    let Ok(original_key) = mz_files::to_object_key(&row.file_fullpath, bucket.as_deref()) else {
        return Ok(json!({"success": false, "message": "Screenshot file is invalid"}));
    };
    if !is_owner_company_key(&original_key, &owner.company_uuid) {
        return Ok(json!({"success": false, "message": "Screenshot file is invalid"}));
    }

    match blur_inner(state, uuid, &row, &original_key).await {
        Ok(v) => Ok(v),
        Err(error) => {
            tracing::error!(%error, module = "screenshot-blur", uuid, "blur failed");
            Ok(json!({"success": false, "message": "Failed to blur image"}))
        }
    }
}

/// `GET /screenshots/:uuid/open-url` — Node: `getOpenUrl`.
pub async fn get_open_url(state: &AppState, uuid: &str, user_id: i32) -> ApiResult<Value> {
    let (row, requester) = tokio::try_join!(
        async { fetch_shot(&state.db, uuid).await.map_err(ApiError::from) },
        async {
            load_file_requester(&state.db, user_id)
                .await
                .map_err(ApiError::from)
        },
    )?;
    let Some(row) = row.filter(|s| s.status != STATUS_DELETED) else {
        return Ok(
            json!({"success": false, "message": "Screenshot not found", "data": Value::Null}),
        );
    };

    // Cross-tenant guard: requester wajib di company yang sama dengan pemilik screenshot.
    let Some(owner_company) = row
        .owner_company_id
        .filter(|c| Some(*c) == requester.company_id)
    else {
        return Ok(json!({
            "success": false,
            "message": "Access denied: screenshot does not belong to your company",
            "data": Value::Null,
        }));
    };

    // FR-032: pemilik, role company-wide company pemilik, atau Division Leader pemiliknya.
    let access =
        can_view_screenshot(&state.db, row.owner_user_id, owner_company, &requester).await?;
    if access == View::DenyLeader {
        return Err(scope_forbidden());
    }
    if access != View::Allow {
        return Ok(json!({
            "success": false,
            "message": "Access denied: screenshot does not belong to you",
            "data": Value::Null,
        }));
    }

    let open_url = screenshot_image_path(uuid, "original");
    if !row.is_blur {
        return Ok(
            json!({"success": true, "message": "Screenshot is not blurred", "data": {"openUrl": open_url}}),
        );
    }
    if !row.blur_role_allows(requester.role_code.as_deref()) {
        return Ok(json!({
            "success": false,
            "message": "Access denied: your role is not allowed to view open image",
            "data": Value::Null,
        }));
    }
    Ok(
        json!({"success": true, "message": "Success get open image url", "data": {"openUrl": open_url}}),
    )
}

/// `screenshotRetentionDays` 1..36499 = tier gratis (kompresi berlaku).
fn is_free_tier(retention: Option<i32>) -> bool {
    retention.is_some_and(|r| r > 0 && r < 36500)
}

/// Pilih referensi file (`original` / `display`) — bagian murni `resolveImageKey`.
fn pick_ref<'a>(
    row: &'a ShotRow,
    variant: &str,
    role_code: Option<&str>,
) -> Result<&'a str, ApiError> {
    if variant == "original" {
        if row.is_blur {
            if !row.blur_role_allows(role_code) {
                return Err(ApiError::forbidden(
                    "Your role is not allowed to view the original image",
                ));
            }
            // `stripBlurSuffix` dilakukan pemanggil (butuh String).
            return Ok(&row.file_fullpath);
        }
        return Ok(&row.file_fullpath);
    }
    Ok(
        match (
            &row.compressed_file_key,
            is_free_tier(row.retention_days) && row.is_compressed,
        ) {
            (Some(k), true) if !k.is_empty() => k,
            _ => &row.file_fullpath,
        },
    )
}

/// Bagian `streamImage` yang memutuskan akses + key — Node: `resolveImageKey`.
pub async fn resolve_image_key(
    state: &AppState,
    uuid: &str,
    variant: &str,
    requester_id: i32,
) -> ApiResult<String> {
    let (row, requester) = tokio::try_join!(
        async { fetch_shot(&state.db, uuid).await.map_err(ApiError::from) },
        async {
            load_file_requester(&state.db, requester_id)
                .await
                .map_err(ApiError::from)
        },
    )?;
    let row = row.filter(|s| s.status == STATUS_FINISHED);
    let owner = row.as_ref().and_then(ShotRow::owner);
    let (Some(row), Some(owner)) = (row, owner) else {
        return Err(not_found());
    };

    let access =
        can_view_screenshot(&state.db, owner.user_id, owner.company_id, &requester).await?;
    if access == View::DenyLeader {
        return Err(scope_forbidden());
    }
    if access != View::Allow {
        return Err(not_found());
    }

    let picked = pick_ref(&row, variant, requester.role_code.as_deref())?;
    let reference = if variant == "original" && row.is_blur {
        strip_blur_suffix(picked)
    } else {
        picked.to_owned()
    };

    let bucket = state.config.var("GCP_BUCKET_NAME");
    let key = mz_files::to_object_key(&reference, bucket.as_deref()).map_err(|_| not_found())?;
    if !is_owner_company_key(&key, &owner.company_uuid) {
        tracing::warn!("Screenshot {uuid}: key outside owner company prefix, refusing to serve");
        return Err(not_found());
    }
    Ok(key)
}

/// `GET /screenshots/:uuid/image` — Node: `streamImage`.
pub async fn stream_image(
    state: &AppState,
    uuid: &str,
    variant: &str,
    requester_id: i32,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    let key = resolve_image_key(state, uuid, variant, requester_id).await?;
    mz_files::stream_object(state, &key, Some(headers), StreamObjectOptions::default()).await
}

/// `GET /screenshots/:uuid/image/url` — Node: `imageUrl`.
pub async fn image_url(
    state: &AppState,
    uuid: &str,
    variant: &str,
    requester_id: i32,
) -> ApiResult<Value> {
    let key = resolve_image_key(state, uuid, variant, requester_id).await?;
    let signed = mz_files::get_signed_object_url(
        state,
        &key,
        &SignedObjectOpts::default(),
        chrono::Utc::now().timestamp_millis(),
    )
    .await?;
    serde_json::to_value(signed).map_err(ApiError::internal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> ShotRow {
        ShotRow {
            id: 1,
            uuid: "u".into(),
            file_fullpath: "companies/c/a-blurred-1.jpg".into(),
            is_blur: true,
            taken_at: JsDate::now(),
            tracking_timeline_id: 1,
            project_id: 1,
            created_at: JsDate::now(),
            updated_at: None,
            status: STATUS_FINISHED,
            compressed_file_key: Some("compressed/companies/c/a.jpg".into()),
            is_compressed: true,
            compressed_at: None,
            project_uuid: Some("p".into()),
            owner_user_id: 1,
            owner_user_uuid: "ou".into(),
            owner_company_id: Some(9),
            owner_company_uuid: Some("c".into()),
            image_blur_enabled: Some(true),
            image_blur_roles: Some(vec!["ADMIN".into()]),
            retention_days: Some(7),
        }
    }

    fn req(user: i32, company: Option<i32>, role: Option<&str>) -> FileRequester {
        FileRequester {
            user_id: user,
            company_id: company,
            role_code: role.map(str::to_owned),
        }
    }

    #[test]
    fn display_uses_compressed_only_for_free_tier() {
        let mut r = row();
        assert_eq!(
            pick_ref(&r, "display", None).ok(),
            Some("compressed/companies/c/a.jpg")
        );
        r.retention_days = Some(36500);
        assert_eq!(
            pick_ref(&r, "display", None).ok(),
            Some("companies/c/a-blurred-1.jpg")
        );
        r.retention_days = None;
        assert_eq!(
            pick_ref(&r, "display", None).ok(),
            Some("companies/c/a-blurred-1.jpg")
        );
        r.retention_days = Some(7);
        r.is_compressed = false;
        assert_eq!(
            pick_ref(&r, "display", None).ok(),
            Some("companies/c/a-blurred-1.jpg")
        );
    }

    #[test]
    fn original_of_blurred_needs_blur_role() {
        let mut r = row();
        assert!(pick_ref(&r, "original", Some("STAFF")).is_err());
        assert!(pick_ref(&r, "original", None).is_err());
        assert!(pick_ref(&r, "original", Some("ADMIN")).is_ok());
        r.image_blur_enabled = Some(false);
        assert!(pick_ref(&r, "original", Some("ADMIN")).is_err());
        r.is_blur = false;
        assert!(pick_ref(&r, "original", Some("STAFF")).is_ok());
    }

    #[test]
    fn leader_decision_branches() {
        let div = ViewerScope::Division {
            user_ids: vec![5, 7],
        };
        // pemilik (user 7) berada di jangkauan Leader 5
        assert_eq!(
            decide_view(7, 10, &req(5, Some(10), Some("STAFF")), || Some(
                div.clone()
            )),
            View::Allow
        );
        assert_eq!(
            decide_view(8, 10, &req(5, Some(10), Some("STAFF")), || Some(
                div.clone()
            )),
            View::DenyLeader
        );
        assert_eq!(
            decide_view(8, 10, &req(5, Some(11), Some("STAFF")), || Some(
                div.clone()
            )),
            View::Deny
        );
        assert_eq!(
            decide_view(8, 10, &req(5, Some(10), Some("STAFF")), || Some(
                ViewerScope::SelfOnly
            )),
            View::Deny
        );
        assert_eq!(
            decide_view(8, 10, &req(5, Some(10), Some("MANAGER")), || None),
            View::Allow
        );
        assert_eq!(
            decide_view(5, 10, &req(5, None, None), || None),
            View::Allow
        );
    }

    #[test]
    fn owner_requires_company_chain() {
        let mut r = row();
        assert!(r.owner().is_some());
        r.owner_company_id = None;
        assert!(r.owner().is_none());
    }

    #[test]
    fn expected_key_prefers_stored_then_rebuilds() {
        let r = row();
        let owner = r.owner().unwrap_or_else(unreachable_owner);
        assert_eq!(
            expected_key(Some("b"), &r, &owner, "u").as_deref(),
            Some("companies/c/a.jpg")
        );
        let mut r2 = row();
        r2.file_fullpath = String::new();
        assert_eq!(
            expected_key(Some("b"), &r2, &owner, "su").as_deref(),
            Some("companies/c/projects/p/users/ou/screenshots/su.jpg")
        );
        r2.project_uuid = None;
        assert_eq!(expected_key(Some("b"), &r2, &owner, "su"), None);
    }

    fn unreachable_owner() -> ScreenshotOwner {
        ScreenshotOwner {
            user_id: 0,
            user_uuid: String::new(),
            company_id: 0,
            company_uuid: String::new(),
        }
    }

    #[test]
    fn blur_produces_jpeg() {
        let img = image::RgbImage::from_fn(32, 32, |x, y| {
            image::Rgb([(x * 8) as u8, (y * 8) as u8, 128])
        });
        let mut src = Vec::new();
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut src, 90);
        let mut enc = enc;
        assert!(
            enc.encode_image(&image::DynamicImage::ImageRgb8(img))
                .is_ok()
        );
        let out = blur_jpeg(&src).unwrap_or_default();
        assert!(out.starts_with(&[0xFF, 0xD8]), "hasil harus JPEG");
        assert!(blur_jpeg(b"not an image").is_err());
    }
}
