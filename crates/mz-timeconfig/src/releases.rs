//! Port `src/app-releases` (9 route: 4 publik `AppReleasesController`, 5 operator
//! `AdminAppReleasesController`). Installer berukuran besar TIDAK di-buffer: unggahan
//! di-spool ke berkas sementara sambil menghitung SHA-512 lalu di-stream ke GCS;
//! unduhan di-stream dari GCS lewat `mz_files::stream_object`.

use std::path::PathBuf;

use axum::Json;
use axum::extract::{Multipart, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use mz_core::extract::{PathParams, Validate, ValidatedQuery};
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, ApiResult, AppState};
use mz_files::{
    StreamObjectOptions, delete_file,
    release_url::{platform_key, release_download_url},
    stream_object, upload_local_file,
};
use mz_projects::dto::PaginatorQuery;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha512};
use tokio::io::AsyncWriteExt;

use crate::operator::{AuditCtx, Operator, record_action};
use crate::util::failure;

pub const GCS_RELEASE_FOLDER: &str = "app-releases";
/// `FileInterceptor` limit.
pub const MAX_UPLOAD_BYTES: u64 = 500 * 1024 * 1024;

/// `PLATFORM_MAP` (kunci URL → enum).
pub fn platform_enum(key: &str) -> Option<&'static str> {
    match key {
        "win" => Some("WINDOWS"),
        "mac" => Some("MAC"),
        "linux" => Some("LINUX"),
        _ => None,
    }
}

/// `CHANNEL_FILE_MAP`.
pub fn channel_of_file(file: &str) -> Option<&'static str> {
    match file {
        "latest.yml" | "latest-mac.yml" | "latest-linux.yml" => Some("LATEST"),
        "beta.yml" | "beta-mac.yml" | "beta-linux.yml" => Some("BETA"),
        "alpha.yml" | "alpha-mac.yml" | "alpha-linux.yml" => Some("ALPHA"),
        _ => None,
    }
}

const RELEASE_COLUMNS: &str = r#"id, uuid::text AS uuid, version, platform::text AS platform,
    channel::text AS channel, "fileName" AS file_name, "fileSize" AS file_size, sha512,
    "downloadUrl" AS download_url, "gcsPath" AS gcs_path, "releaseNotes" AS release_notes,
    "releaseDate" AS release_date, "isActive" AS is_active, "createdAt" AS created_at,
    "updatedAt" AS updated_at"#;

/// Baris `appRelease` (semua kolom skalar).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ReleaseRow {
    pub id: i32,
    pub uuid: String,
    pub version: String,
    pub platform: String,
    pub channel: String,
    pub file_name: String,
    pub file_size: i64,
    pub sha512: String,
    pub download_url: String,
    pub gcs_path: String,
    pub release_notes: Option<String>,
    pub release_date: JsDate,
    pub is_active: bool,
    pub created_at: JsDate,
    pub updated_at: Option<JsDate>,
}

impl ReleaseRow {
    fn effective_url(&self, state: &AppState) -> String {
        release_download_url(state, &self.platform, &self.version, &self.file_name)
            .unwrap_or_else(|| self.download_url.clone())
    }

    /// `{...r, fileSize: string, downloadUrl: proxy}`.
    pub fn to_json(&self, state: &AppState) -> Value {
        json!({
            "id": self.id,
            "uuid": self.uuid,
            "version": self.version,
            "platform": self.platform,
            "channel": self.channel,
            "fileName": self.file_name,
            "fileSize": self.file_size.to_string(),
            "sha512": self.sha512,
            "downloadUrl": self.effective_url(state),
            "gcsPath": self.gcs_path,
            "releaseNotes": self.release_notes,
            "releaseDate": self.release_date,
            "isActive": self.is_active,
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
        })
    }
}

// ───────────────────────── publik ─────────────────────────

/// Node: src/app-releases/app-releases.service.ts:400 (`getChangelog`)
pub async fn changelog(state: &AppState) -> ApiResult<Value> {
    let rows: Vec<(String, Option<String>, JsDate)> = sqlx::query_as(
        r#"SELECT version, "releaseNotes", "releaseDate" FROM (
             SELECT DISTINCT ON (version) version, "releaseNotes", "releaseDate"
             FROM "appRelease" WHERE "isActive" = true
             ORDER BY version, "releaseDate" DESC, id DESC) d
           ORDER BY "releaseDate" DESC LIMIT 5"#,
    )
    .fetch_all(&state.db)
    .await?;
    let data: Vec<Value> = rows
        .into_iter()
        .map(|(version, notes, date)| {
            json!({ "version": version, "releaseNotes": notes, "releaseDate": date })
        })
        .collect();
    Ok(json!({ "success": true, "message": "Success get changelog", "data": data }))
}

/// Node: src/app-releases/app-releases.service.ts:425 (`getReleaseNotes`)
pub async fn release_notes(
    state: &AppState,
    platform: &str,
    channel: Option<&str>,
) -> ApiResult<Value> {
    let Some(platform_enum) = platform_enum(platform) else {
        return Ok(json!({ "success": false, "message": "Invalid platform", "data": null }));
    };
    let channel = channel
        .map(str::to_uppercase)
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "LATEST".to_owned());
    // Enum Prisma tak dikenal → validation error (500).
    if !["LATEST", "BETA", "ALPHA"].contains(&channel.as_str()) {
        return Err(ApiError::internal(anyhow::anyhow!(
            "invalid ReleaseChannel: {channel}"
        )));
    }
    let row: Option<(String, String, String, Option<String>, JsDate)> = sqlx::query_as(
        r#"SELECT version, platform::text, channel::text, "releaseNotes", "releaseDate"
           FROM "appRelease"
           WHERE platform = $1::"ReleasePlatform" AND channel = $2::"ReleaseChannel" AND "isActive" = true
           ORDER BY "releaseDate" DESC LIMIT 1"#,
    )
    .bind(platform_enum)
    .bind(&channel)
    .fetch_optional(&state.db)
    .await?;
    Ok(match row {
        None => json!({ "success": false, "message": "No release found", "data": null }),
        Some((version, platform, channel, notes, date)) => json!({
            "success": true,
            "message": "Success get release notes",
            "data": {
                "version": version, "platform": platform, "channel": channel,
                "releaseNotes": notes, "releaseDate": date,
            },
        }),
    })
}

/// Node: src/app-releases/app-releases.service.ts:341 (`getUpdateYaml`)
pub async fn update_yaml(
    state: &AppState,
    platform: &str,
    channel_file: &str,
) -> ApiResult<Option<String>> {
    let (Some(platform_enum), Some(channel)) =
        (platform_enum(platform), channel_of_file(channel_file))
    else {
        return Ok(None);
    };
    let release: Option<ReleaseRow> = sqlx::query_as(&format!(
        r#"SELECT {RELEASE_COLUMNS} FROM "appRelease"
           WHERE platform = $1::"ReleasePlatform" AND channel = $2::"ReleaseChannel" AND "isActive" = true
           ORDER BY "releaseDate" DESC LIMIT 1"#
    ))
    .bind(platform_enum)
    .bind(channel)
    .fetch_optional(&state.db)
    .await?;
    Ok(release.map(|r| build_yaml(&r, &r.effective_url(state))))
}

/// Isi YAML electron-updater (`lines.join('\n')`).
pub fn build_yaml(r: &ReleaseRow, download_url: &str) -> String {
    let mut lines = vec![
        format!("version: {}", r.version),
        "files:".to_owned(),
        format!("  - url: {download_url}"),
        format!("    sha512: {}", r.sha512),
        format!("    size: {}", r.file_size),
        format!("path: {download_url}"),
        format!("sha512: {}", r.sha512),
        format!("releaseDate: '{}'", r.release_date.to_js_string()),
    ];
    if let Some(notes) = r.release_notes.as_deref().filter(|n| !n.is_empty()) {
        lines.push(format!("releaseNotes: '{}'", notes.replace('\'', "''")));
    }
    lines.join("\n")
}

/// Node: src/app-releases/app-releases.service.ts:381 (`streamDownload`)
pub async fn stream_download(
    state: &AppState,
    platform: &str,
    version: &str,
    filename: &str,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    let not_found = || ApiError::not_found("Release not found");
    let platform_enum = platform_enum(platform).ok_or_else(not_found)?;
    // Kolom `version` VarChar(50), `fileName` VarChar(500) (panjang dalam karakter, seperti JS `.length`).
    if version.encode_utf16().count() > 50 || filename.encode_utf16().count() > 500 {
        return Err(not_found());
    }
    let release: Option<(String, String)> = sqlx::query_as(
        r#"SELECT "gcsPath", "fileName" FROM "appRelease"
           WHERE platform = $1::"ReleasePlatform" AND version = $2 AND "fileName" = $3 AND "isActive" = true
           ORDER BY "releaseDate" DESC LIMIT 1"#,
    )
    .bind(platform_enum)
    .bind(version)
    .bind(filename)
    .fetch_optional(&state.db)
    .await?;
    let (gcs_path, file_name) = release.ok_or_else(not_found)?;
    // Key SELALU `gcsPath` dari DB; `filename` URL hanya dicocokkan.
    stream_object(
        state,
        &gcs_path,
        Some(headers),
        StreamObjectOptions {
            cache_control: Some("public, max-age=300".to_owned()),
            filename: Some(file_name),
            force_attachment: true,
            info: None,
        },
    )
    .await
}

// ───────────────────────── operator ─────────────────────────

/// Query `GET /admin/app-releases` (`QueryAppReleaseDto` = `PaginatorDto` + platform/channel).
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AdminListQuery {
    pub page_number: Option<String>,
    pub items_per_page: Option<String>,
    pub order_by: Option<String>,
    pub order_column: Option<String>,
    pub search: Option<String>,
    pub platform: Option<String>,
    pub channel: Option<String>,
}

impl AdminListQuery {
    fn paginator(&self) -> PaginatorQuery {
        PaginatorQuery {
            page_number: self.page_number.clone(),
            items_per_page: self.items_per_page.clone(),
            order_by: self.order_by.clone(),
            order_column: self.order_column.clone(),
            search: self.search.clone(),
        }
    }
}

impl Validate for AdminListQuery {
    fn validate(&self) -> Vec<String> {
        // Properti kelas turunan lebih dulu, lalu kelas induk (`PaginatorDto`).
        let mut out = Vec::new();
        if self
            .platform
            .as_deref()
            .is_some_and(|p| !["WINDOWS", "MAC", "LINUX"].contains(&p))
        {
            out.push(ENUM_PLATFORM_MSG.into());
        }
        if self
            .channel
            .as_deref()
            .is_some_and(|c| !["LATEST", "BETA", "ALPHA"].contains(&c))
        {
            out.push(ENUM_CHANNEL_MSG.into());
        }
        out.extend(self.paginator().validate());
        out
    }
}

const ORDER_COLUMNS: &[&str] = &[
    "id",
    "uuid",
    "version",
    "platform",
    "channel",
    "fileName",
    "fileSize",
    "sha512",
    "downloadUrl",
    "gcsPath",
    "releaseNotes",
    "releaseDate",
    "isActive",
    "createdAt",
    "updatedAt",
];

fn prisma_error(what: &str) -> ApiError {
    ApiError::internal(anyhow::anyhow!("prisma validation error: {what}"))
}

/// Node: src/app-releases/app-releases.service.ts:122 (`findAll`)
pub async fn find_all(state: &AppState, q: &AdminListQuery) -> ApiResult<Value> {
    let p = q.paginator();
    let (page, per_page) = if p.page_number.is_none() && p.items_per_page.is_none() {
        (1, 20)
    } else {
        (
            p.page_number().ok_or_else(|| prisma_error("skip is NaN"))?,
            p.items_per_page()
                .ok_or_else(|| prisma_error("take is NaN"))?,
        )
    };
    let order_by = q
        .order_by
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "desc".into());
    let order_column = q
        .order_column
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "createdAt".into());
    if !ORDER_COLUMNS.contains(&order_column.as_str()) {
        return Err(prisma_error("unknown orderBy column"));
    }
    let direction = match order_by.as_str() {
        "asc" => "ASC",
        "desc" => "DESC",
        _ => return Err(prisma_error("invalid sort order")),
    };
    let skip = (page - 1).saturating_mul(per_page);
    if skip < 0 || per_page < 0 {
        return Err(prisma_error("negative take/skip"));
    }

    let search = q.search.as_deref().filter(|s| !s.is_empty());
    // Prisma `contains` tanpa mode → LIKE '%x%' (tanpa escape metakarakter).
    let where_sql = r#" WHERE ($1::text IS NULL OR platform = $1::"ReleasePlatform")
        AND ($2::text IS NULL OR channel = $2::"ReleaseChannel")
        AND ($3::text IS NULL OR version LIKE '%' || $3 || '%')"#;
    let total: i64 =
        sqlx::query_scalar(&format!(r#"SELECT COUNT(*) FROM "appRelease"{where_sql}"#))
            .bind(q.platform.as_deref())
            .bind(q.channel.as_deref())
            .bind(search)
            .fetch_one(&state.db)
            .await?;
    // `order_column`/`direction` dari daftar putih.
    let rows: Vec<ReleaseRow> = sqlx::query_as(&format!(
        r#"SELECT {RELEASE_COLUMNS} FROM "appRelease"{where_sql}
           ORDER BY "{order_column}" {direction}, id {direction} LIMIT $4 OFFSET $5"#
    ))
    .bind(q.platform.as_deref())
    .bind(q.channel.as_deref())
    .bind(search)
    .bind(per_page)
    .bind(skip)
    .fetch_all(&state.db)
    .await?;
    Ok(json!({
        "success": true,
        "message": "Success get all releases",
        "data": {
            "total": total, "itemsPerPage": per_page, "pageNumber": page,
            "result": rows.iter().map(|r| r.to_json(state)).collect::<Vec<_>>(),
        },
    }))
}

/// Node: src/app-releases/app-releases.service.ts:166 (`findOne`)
pub async fn find_one(state: &AppState, uuid: &str) -> Value {
    let fail = || failure("Failed to get release details");
    // Kolom `@db.Uuid`: string yang ditolak Prisma melempar → catch.
    let Ok(uuid) = mz_core::prisma::parse_uuid(uuid) else {
        return fail();
    };
    let row: sqlx::Result<Option<ReleaseRow>> = sqlx::query_as(&format!(
        r#"SELECT {RELEASE_COLUMNS} FROM "appRelease" WHERE uuid = $1::uuid LIMIT 1"#
    ))
    .bind(uuid)
    .fetch_optional(&state.db)
    .await;
    match row {
        Ok(None) => failure("Release not found"),
        Ok(Some(r)) => {
            json!({ "success": true, "message": "Success get release details", "data": r.to_json(state) })
        }
        Err(err) => {
            tracing::error!(error = %err, "app-releases-findOne");
            fail()
        }
    }
}

/// Node: src/app-releases/app-releases.service.ts:196 (`toggleActive`).
/// `is_active`: `None` = `undefined` (Prisma mengabaikan field); `Some(Err)` = tipe salah.
pub async fn toggle_active(
    state: &AppState,
    uuid: &str,
    is_active: Result<Option<bool>, ()>,
) -> Value {
    let fail = || failure("Failed to toggle release status");
    let Ok(uuid) = mz_core::prisma::parse_uuid(uuid) else {
        return fail();
    };
    let Ok(is_active) = is_active else {
        // Release dicari dulu; tipe salah baru gagal di `updateMany`.
        let exists: sqlx::Result<Option<i32>> =
            sqlx::query_scalar(r#"SELECT id FROM "appRelease" WHERE uuid = $1::uuid LIMIT 1"#)
                .bind(uuid)
                .fetch_optional(&state.db)
                .await;
        return match exists {
            Ok(None) => failure("Release not found"),
            _ => fail(),
        };
    };
    let res: sqlx::Result<Option<i32>> =
        sqlx::query_scalar(r#"SELECT id FROM "appRelease" WHERE uuid = $1::uuid LIMIT 1"#)
            .bind(uuid)
            .fetch_optional(&state.db)
            .await;
    match res {
        Ok(None) => return failure("Release not found"),
        Ok(Some(_)) => {}
        Err(err) => {
            tracing::error!(error = %err, "app-releases-toggle");
            return fail();
        }
    }
    if let Some(flag) = is_active
        && let Err(err) =
            sqlx::query(r#"UPDATE "appRelease" SET "isActive" = $2 WHERE uuid = $1::uuid"#)
                .bind(uuid)
                .bind(flag)
                .execute(&state.db)
                .await
    {
        tracing::error!(error = %err, "app-releases-toggle");
        return fail();
    }
    let activated = is_active == Some(true);
    let mut data = Map::new();
    data.insert("uuid".into(), json!(uuid));
    if let Some(flag) = is_active {
        data.insert("isActive".into(), json!(flag));
    }
    json!({
        "success": true,
        "message": format!("Release {} successfully", if activated { "activated" } else { "deactivated" }),
        "data": data,
    })
}

/// Node: src/app-releases/app-releases.service.ts:231 (`deleteRelease`)
pub async fn delete_release(state: &AppState, uuid: &str) -> Value {
    let fail = || failure("Failed to delete release");
    let Ok(uuid) = mz_core::prisma::parse_uuid(uuid) else {
        return fail();
    };
    let found: sqlx::Result<Option<String>> =
        sqlx::query_scalar(r#"SELECT "gcsPath" FROM "appRelease" WHERE uuid = $1::uuid LIMIT 1"#)
            .bind(uuid)
            .fetch_optional(&state.db)
            .await;
    let gcs_path = match found {
        Ok(Some(p)) => p,
        Ok(None) => return failure("Release not found"),
        Err(err) => {
            tracing::error!(error = %err, "app-releases-delete");
            return fail();
        }
    };
    // File mungkin sudah terhapus di GCS: lanjut hapus baris DB.
    let _ = delete_file(state, &gcs_path).await;
    match sqlx::query(r#"DELETE FROM "appRelease" WHERE uuid = $1::uuid"#)
        .bind(uuid)
        .execute(&state.db)
        .await
    {
        Ok(_) => {
            json!({ "success": true, "message": "Release deleted successfully", "data": null })
        }
        Err(err) => {
            tracing::error!(error = %err, "app-releases-delete");
            fail()
        }
    }
}

// ───────────────────────── upload ─────────────────────────

/// Berkas sementara yang dihapus saat di-drop.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct Spooled {
    _guard: TempFile,
    path: PathBuf,
    original_name: String,
    mime: String,
    size: u64,
    sha512_b64: String,
}

/// Hasil membaca multipart: berkas (bila ada) + field teks.
struct UploadForm {
    file: Option<Spooled>,
    fields: Map<String, Value>,
}

/// `file.originalname` — multer membuang direktori dari nama berkas.
fn base_name(name: &str) -> String {
    name.rsplit(['/', '\\']).next().unwrap_or(name).to_owned()
}

/// `@UseInterceptors(FileInterceptor('file'))`: multer hanya membaca `multipart/form-data`.
/// Content-type lain dilewati multer: `@Body()` = hasil `express.json` / `express.urlencoded`
/// (lainnya `{}`) dan `file` = undefined, lalu tetap masuk `ValidationPipe` yang sama.
async fn read_upload(req: axum::extract::Request) -> ApiResult<UploadForm> {
    use axum::extract::FromRequest;
    let ct = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if ct.starts_with("multipart/form-data") {
        let mp = Multipart::from_request(req, &())
            .await
            .map_err(|e| ApiError::message(e.status(), e.body_text()))?;
        return read_form(mp).await;
    }
    let is_form = ct.starts_with("application/x-www-form-urlencoded");
    let is_json = mz_core::extract::is_json_content_type(req.headers());
    let bytes = axum::body::Bytes::from_request(req, &())
        .await
        .map_err(mz_core::extract::bytes_rejection)?;
    // `express.json` / `express.urlencoded` di main.ts: limit 100kb.
    if (is_json || is_form) && bytes.len() > 100 * 1024 {
        return Err(mz_core::extract::payload_too_large());
    }
    let fields = if is_form {
        mz_core::qs::parse(&String::from_utf8_lossy(&bytes))
            .into_iter()
            .map(|(k, v)| {
                let v = match v {
                    mz_core::qs::QVal::One(s) => Value::String(s),
                    mz_core::qs::QVal::Many(xs) => {
                        Value::Array(xs.into_iter().map(Value::String).collect())
                    }
                    mz_core::qs::QVal::Obj => Value::Object(Map::new()),
                };
                (k, v)
            })
            .collect()
    } else {
        // Array JSON → `ValidationPipe` memperlakukannya sebagai objek kosong.
        match mz_core::extract::parse_express_json(is_json, &bytes)? {
            Value::Object(m) => m,
            _ => Map::new(),
        }
    };
    Ok(UploadForm { file: None, fields })
}

async fn read_form(mut mp: Multipart) -> ApiResult<UploadForm> {
    let mut form = UploadForm {
        file: None,
        fields: Map::new(),
    };
    while let Some(mut field) = mp
        .next_field()
        .await
        .map_err(|e| ApiError::message(e.status(), e.body_text()))?
    {
        let name = field.name().unwrap_or_default().to_owned();
        if name == "file" && field.file_name().is_some() {
            let original = base_name(field.file_name().unwrap_or_default());
            let mime = field
                .content_type()
                .filter(|m| !m.is_empty())
                .unwrap_or("application/octet-stream")
                .to_owned();
            let path = std::env::temp_dir().join(format!("mz-release-{}", uuid::Uuid::new_v4()));
            let guard = TempFile(path.clone());
            let mut out = tokio::fs::File::create(&path)
                .await
                .map_err(ApiError::internal)?;
            let mut hasher = Sha512::new();
            let mut size: u64 = 0;
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(|e| ApiError::message(e.status(), e.body_text()))?
            {
                size += chunk.len() as u64;
                if size > MAX_UPLOAD_BYTES {
                    // multer `LIMIT_FILE_SIZE` → 413 di Nest.
                    return Err(ApiError::message(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "File too large",
                    ));
                }
                hasher.update(&chunk);
                out.write_all(&chunk).await.map_err(ApiError::internal)?;
            }
            out.flush().await.map_err(ApiError::internal)?;
            if form.file.is_none() {
                form.file = Some(Spooled {
                    _guard: guard,
                    path,
                    original_name: original,
                    mime,
                    size,
                    sha512_b64: STANDARD.encode(hasher.finalize()),
                });
            }
        } else {
            let text = field
                .text()
                .await
                .map_err(|e| ApiError::message(e.status(), e.body_text()))?;
            form.fields.insert(name, Value::String(text));
        }
    }
    Ok(form)
}

// `@IsEnum([...])` dengan ARRAY: class-validator menyusun daftar dari `Object.keys` yang
// bukan angka → indeks array dibuang → daftar kosong (nilai tetap dicek terhadap isinya).
const ENUM_PLATFORM_MSG: &str = "platform must be one of the following values: ";
const ENUM_CHANNEL_MSG: &str = "channel must be one of the following values: ";

/// `isNotEmpty` class-validator: gagal hanya untuk `''`, `null`, `undefined`.
fn is_empty_value(v: Option<&Value>) -> bool {
    matches!(v, None | Some(Value::Null)) || v.and_then(Value::as_str) == Some("")
}

/// Validasi `CreateAppReleaseDto`.
pub fn validate_create(m: &Map<String, Value>) -> Vec<String> {
    let mut out = Vec::new();
    let s = |k: &str| m.get(k).and_then(Value::as_str);
    if is_empty_value(m.get("version")) {
        out.push("version should not be empty".to_owned());
    }
    if s("version").is_none() {
        out.push("version must be a string".to_owned());
    }
    if is_empty_value(m.get("platform")) {
        out.push("platform should not be empty".to_owned());
    }
    if !s("platform").is_some_and(|p| ["WINDOWS", "MAC", "LINUX"].contains(&p)) {
        out.push(ENUM_PLATFORM_MSG.to_owned());
    }
    if let Some(v) = m.get("channel").filter(|v| !v.is_null())
        && !v
            .as_str()
            .is_some_and(|c| ["LATEST", "BETA", "ALPHA"].contains(&c))
    {
        out.push(ENUM_CHANNEL_MSG.to_owned());
    }
    if let Some(v) = m.get("releaseNotes").filter(|v| !v.is_null())
        && !v.is_string()
    {
        out.push("releaseNotes must be a string".to_owned());
    }
    out
}

async fn upload_inner(
    state: &AppState,
    file: Option<&Spooled>,
    m: &Map<String, Value>,
) -> Result<Value, String> {
    let s = |k: &str| {
        m.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let platform = s("platform");
    let version = s("version");
    let channel = Some(s("channel"))
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "LATEST".into());
    let notes = Some(s("releaseNotes")).filter(|n| !n.is_empty());
    let platform_key = platform_key(&platform);

    let Some(file) = file else {
        return Err("Cannot read properties of undefined (reading 'buffer')".into());
    };
    let gcs_path = format!(
        "{GCS_RELEASE_FOLDER}/{}/{}/{}",
        platform_key.unwrap_or("undefined"),
        channel.to_lowercase(),
        file.original_name
    );

    let bucket = mz_files::gcs::default_bucket(state).map_err(|e| e.to_string())?;
    upload_local_file(state, &file.path, &gcs_path, &bucket, Some(&file.mime))
        .await
        .map_err(|e| e.to_string())?;

    let download_url = release_download_url(state, &platform, &version, &file.original_name)
        .ok_or_else(|| format!("invalid platform {platform}"))?;

    let now = now_naive();
    let row: ReleaseRow = sqlx::query_as(&format!(
        r#"INSERT INTO "appRelease"
             (version, platform, channel, "fileName", "fileSize", sha512, "downloadUrl", "gcsPath",
              "releaseNotes", "releaseDate", "isActive")
           VALUES ($1, $2::"ReleasePlatform", $3::"ReleaseChannel", $4, $5, $6, $7, $8, $9, $10, true)
           ON CONFLICT (version, platform, channel) DO UPDATE SET
             "fileName" = EXCLUDED."fileName", "fileSize" = EXCLUDED."fileSize", sha512 = EXCLUDED.sha512,
             "downloadUrl" = EXCLUDED."downloadUrl", "gcsPath" = EXCLUDED."gcsPath",
             "releaseNotes" = EXCLUDED."releaseNotes", "releaseDate" = $10, "isActive" = true
           RETURNING {RELEASE_COLUMNS}"#
    ))
    .bind(&version)
    .bind(&platform)
    .bind(&channel)
    .bind(&file.original_name)
    .bind(i64::try_from(file.size).unwrap_or(i64::MAX))
    .bind(&file.sha512_b64)
    .bind(&download_url)
    .bind(&gcs_path)
    .bind(&notes)
    .bind(now)
    .fetch_one(&state.db)
    .await
    .map_err(|e| e.to_string())?;

    // Tabel `latestVersion` (satu baris).
    let column = match platform.as_str() {
        "WINDOWS" => "windows_url",
        "MAC" => "mac_url",
        _ => "linux_url",
    };
    let existing: Option<i32> =
        sqlx::query_scalar(r#"SELECT id FROM "latestVersion" ORDER BY id LIMIT 1"#)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| e.to_string())?;
    // `column` dari daftar putih di atas.
    let res = match existing {
        Some(id) => {
            sqlx::query(&format!(
                r#"UPDATE "latestVersion" SET version = $2, {column} = $3 WHERE id = $1"#
            ))
            .bind(id)
            .bind(&version)
            .bind(&download_url)
            .execute(&state.db)
            .await
        }
        None => {
            sqlx::query(&format!(
                r#"INSERT INTO "latestVersion" (version, {column}) VALUES ($1, $2)"#
            ))
            .bind(&version)
            .bind(&download_url)
            .execute(&state.db)
            .await
        }
    };
    res.map_err(|e| e.to_string())?;

    Ok(json!({
        "success": true,
        "message": "Release uploaded successfully",
        "data": {
            "uuid": row.uuid, "version": row.version, "platform": row.platform,
            "channel": row.channel, "fileName": row.file_name,
            "fileSize": row.file_size.to_string(), "downloadUrl": row.effective_url(state),
        },
    }))
}

// ───────────────────────── handler HTTP ─────────────────────────

fn yaml_response(body: String) -> Response {
    let mut res = (StatusCode::OK, body).into_response();
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/yaml; charset=utf-8"),
    );
    res
}

/// Node: src/app-releases/app-releases.controller.ts:17 (`getChangelog`)
pub async fn h_changelog(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(changelog(&state).await?))
}

/// Query `release-notes` (`@Query('channel')`).
#[derive(Debug, Default, Deserialize)]
pub struct ChannelQuery {
    pub channel: Option<String>,
}

/// Node: src/app-releases/app-releases.controller.ts:22 (`getReleaseNotes`)
pub async fn h_release_notes(
    State(state): State<AppState>,
    PathParams(platform): PathParams<String>,
    Query(q): Query<ChannelQuery>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        release_notes(&state, &platform, q.channel.as_deref()).await?,
    ))
}

/// Node: src/app-releases/app-releases.controller.ts:30 (`getUpdateYaml`)
pub async fn h_update_yaml(
    State(state): State<AppState>,
    PathParams((platform, channel_file)): PathParams<(String, String)>,
) -> ApiResult<Response> {
    match update_yaml(&state, &platform, &channel_file).await? {
        Some(yaml) => Ok(yaml_response(yaml)),
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": "No release found", "data": null })),
        )
            .into_response()),
    }
}

/// Node: src/app-releases/app-releases.controller.ts:51 (`download`)
pub async fn h_download(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParams((platform, version, filename)): PathParams<(String, String, String)>,
) -> ApiResult<Response> {
    stream_download(&state, &platform, &version, &filename, &headers).await
}

/// Node: src/app-releases/admin-app-releases.controller.ts:33 (`uploadRelease`, 201)
pub async fn h_upload(
    State(state): State<AppState>,
    op: Operator,
    audit: AuditCtx,
    req: axum::extract::Request,
) -> ApiResult<Response> {
    let form = read_upload(req).await?;
    let errors = validate_create(&form.fields);
    if !errors.is_empty() {
        return Err(ApiError::Validation(errors));
    }
    let out = match upload_inner(&state, form.file.as_ref(), &form.fields).await {
        Ok(v) => v,
        Err(msg) => {
            tracing::error!(error = %msg, "app-releases-upload");
            json!({ "success": false, "message": format!("Failed to upload release: {msg}"), "data": null })
        }
    };
    if out.get("success") != Some(&Value::Bool(false)) {
        record_action(
            &state,
            &op,
            &audit,
            "app_release.upload",
            None,
            Some(&Value::Object(form.fields)),
        )
        .await;
    }
    Ok((StatusCode::CREATED, Json(out)).into_response())
}

/// Node: src/app-releases/admin-app-releases.controller.ts:50 (`findAll`)
pub async fn h_admin_list(
    State(state): State<AppState>,
    _op: Operator,
    ValidatedQuery(q): ValidatedQuery<AdminListQuery>,
) -> ApiResult<Json<Value>> {
    Ok(Json(find_all(&state, &q).await?))
}

/// Node: src/app-releases/admin-app-releases.controller.ts:57 (`findOne`)
pub async fn h_admin_one(
    State(state): State<AppState>,
    _op: Operator,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(find_one(&state, &uuid).await))
}

/// `GET /admin/app-releases/upload`: path statis ditelan `GET :uuid` di Node.
pub async fn h_admin_one_upload(
    State(state): State<AppState>,
    _op: Operator,
) -> ApiResult<Json<Value>> {
    Ok(Json(find_one(&state, "upload").await))
}

/// `DELETE /admin/app-releases/upload`: ditelan `DELETE :uuid` di Node.
pub async fn h_delete_upload(
    State(state): State<AppState>,
    op: Operator,
    audit: AuditCtx,
) -> ApiResult<Json<Value>> {
    let out = delete_release(&state, "upload").await;
    if out.get("success") != Some(&Value::Bool(false)) {
        record_action(
            &state,
            &op,
            &audit,
            "app_release.delete",
            Some("upload"),
            None,
        )
        .await;
    }
    Ok(Json(out))
}

/// Node: src/app-releases/admin-app-releases.controller.ts:64 (`toggleActive`)
pub async fn h_toggle(
    State(state): State<AppState>,
    op: Operator,
    audit: AuditCtx,
    PathParams(uuid): PathParams<String>,
    body: Option<Json<Value>>,
) -> ApiResult<Json<Value>> {
    let body = body.map(|b| b.0).unwrap_or(Value::Null);
    let is_active = match body.get("isActive") {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(()),
    };
    let out = toggle_active(&state, &uuid, is_active).await;
    if out.get("success") != Some(&Value::Bool(false)) {
        record_action(
            &state,
            &op,
            &audit,
            "app_release.toggle",
            Some(&uuid),
            Some(&body),
        )
        .await;
    }
    Ok(Json(out))
}

/// Node: src/app-releases/admin-app-releases.controller.ts:76 (`deleteRelease`)
pub async fn h_delete(
    State(state): State<AppState>,
    op: Operator,
    audit: AuditCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Json<Value>> {
    let out = delete_release(&state, &uuid).await;
    if out.get("success") != Some(&Value::Bool(false)) {
        record_action(&state, &op, &audit, "app_release.delete", Some(&uuid), None).await;
    }
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> ReleaseRow {
        ReleaseRow {
            id: 1,
            uuid: "u".into(),
            version: "1.2.3".into(),
            platform: "WINDOWS".into(),
            channel: "LATEST".into(),
            file_name: "Setup 1.2.3.exe".into(),
            file_size: 12345,
            sha512: "abc==".into(),
            download_url: "old".into(),
            gcs_path: "app-releases/win/latest/Setup 1.2.3.exe".into(),
            release_notes: Some("it's fixed".into()),
            release_date: JsDate(
                chrono::NaiveDate::from_ymd_opt(2026, 11, 16)
                    .unwrap_or_default()
                    .and_hms_opt(8, 0, 0)
                    .unwrap_or_default(),
            ),
            is_active: true,
            created_at: JsDate(now_naive()),
            updated_at: None,
        }
    }

    #[test]
    fn maps() {
        assert_eq!(platform_enum("win"), Some("WINDOWS"));
        assert_eq!(platform_enum("x"), None);
        assert_eq!(channel_of_file("beta-mac.yml"), Some("BETA"));
        assert_eq!(channel_of_file("foo.yml"), None);
    }

    #[test]
    fn yaml_matches_electron_updater_shape() {
        let yaml = build_yaml(&row(), "https://api/x");
        assert_eq!(
            yaml,
            "version: 1.2.3\nfiles:\n  - url: https://api/x\n    sha512: abc==\n    size: 12345\npath: https://api/x\nsha512: abc==\nreleaseDate: '2026-11-16T08:00:00.000Z'\nreleaseNotes: 'it''s fixed'"
        );
    }

    #[test]
    fn create_dto_validation() {
        let m = |v: Value| v.as_object().cloned().unwrap_or_default();
        assert_eq!(
            validate_create(&m(json!({}))),
            vec![
                "version should not be empty",
                "version must be a string",
                "platform should not be empty",
                "platform must be one of the following values: ",
            ]
        );
        // Angka bukan "kosong" bagi `IsNotEmpty`; hanya `IsString` yang gagal.
        assert_eq!(
            validate_create(&m(json!({"version": 1, "platform": "MAC"}))),
            vec!["version must be a string"]
        );
        assert!(
            validate_create(&m(json!({"version":"1","platform":"MAC","channel":"BETA"})))
                .is_empty()
        );
        assert_eq!(
            validate_create(&m(json!({"version":"1","platform":"MAC","channel":"x"}))),
            vec!["channel must be one of the following values: "]
        );
    }

    #[test]
    fn base_name_strips_dirs() {
        assert_eq!(base_name("C:\\a\\b.exe"), "b.exe");
        assert_eq!(base_name("../x/y.dmg"), "y.dmg");
        assert_eq!(base_name("z.AppImage"), "z.AppImage");
    }
}
