//! `UserLogsService` (`user-logs.service.ts`): log desktop per user per hari, isi di-gzip dan
//! disimpan di GCS (`filePath`); baris lama masih bisa berisi `content`.

use std::io::{Read, Write};

use chrono::NaiveDate;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use mz_core::time::{JsDate, parse_js_date};
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Value, json};

use mz_files::GcsError;

const LOG_CONTENT_TYPE: &str = "application/gzip";

/// `user-logs/<userId>/<YYYY-MM-DD>/<uuid>.log.gz` — tanggal mengikuti kolom `@db.Date` (UTC).
pub fn user_log_object_key(user_id: i32, date: NaiveDate, uuid: &str) -> String {
    format!(
        "user-logs/{user_id}/{}/{uuid}.log.gz",
        date.format("%Y-%m-%d")
    )
}

/// `parseLogDate`: 400 bila bukan tanggal valid.
pub fn parse_log_date(raw: Option<&str>) -> ApiResult<NaiveDate> {
    raw.and_then(parse_js_date)
        .map(|d| d.date())
        .ok_or_else(|| ApiError::bad_request("date must be a valid date (YYYY-MM-DD)"))
}

pub fn gzip(text: &str) -> std::io::Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(text.as_bytes())?;
    enc.finish()
}

pub fn gunzip(bytes: &[u8]) -> std::io::Result<String> {
    let mut out = Vec::new();
    GzDecoder::new(bytes).read_to_end(&mut out)?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

#[derive(sqlx::FromRow)]
struct LogRow {
    id: i32,
    uuid: String,
    #[sqlx(rename = "userId")]
    user_id: i32,
    date: JsDate,
    content: Option<String>,
    #[sqlx(rename = "filePath")]
    file_path: Option<String>,
    #[sqlx(rename = "createdAt")]
    created_at: JsDate,
    #[sqlx(rename = "updatedAt")]
    updated_at: Option<JsDate>,
}

const COLUMNS: &str = r#"id, uuid::text AS "uuid", "userId", date::timestamp AS date, content,
    "filePath", "createdAt", "updatedAt""#;

/// Bentuk respons: baris tanpa `filePath`, `content` berisi teks utuh.
fn log_json(r: &LogRow, content: Option<&str>) -> Value {
    json!({
        "id": r.id,
        "uuid": r.uuid,
        "userId": r.user_id,
        "date": r.date,
        "content": content,
        "createdAt": r.created_at,
        "updatedAt": r.updated_at,
    })
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> ApiResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)
}

/// `POST /users/log`.
pub async fn create_log(
    state: &AppState,
    user_id: i32,
    date: &str,
    content: &str,
) -> ApiResult<Value> {
    let date = parse_log_date(Some(date))?;
    let uuid = uuid::Uuid::new_v4().to_string();
    let file_path = user_log_object_key(user_id, date, &uuid);

    let text = content.to_owned();
    let compressed = blocking(move || gzip(&text)).await?;
    mz_files::upload_buffer(state, &file_path, compressed, LOG_CONTENT_TYPE).await?;

    let size_bytes = i32::try_from(content.len()).unwrap_or(i32::MAX);
    let inserted = sqlx::query_as::<_, LogRow>(&format!(
        r#"INSERT INTO "userLog" (uuid, "userId", date, "filePath", "sizeBytes")
           VALUES ($1::uuid, $2, $3, $4, $5) RETURNING {COLUMNS}"#
    ))
    .bind(&uuid)
    .bind(user_id)
    .bind(date)
    .bind(&file_path)
    .bind(size_bytes)
    .fetch_one(&state.db)
    .await;
    let row = match inserted {
        Ok(row) => row,
        Err(err) => {
            // Object yang sudah terunggah tidak punya baris; buang supaya tidak yatim.
            if let Err(e) = mz_files::delete_file(state, &file_path).await {
                tracing::warn!(file_path, error = ?e, "Gagal menghapus object log yatim");
            }
            return Err(err.into());
        }
    };
    Ok(json!({ "success": true, "message": "Log saved", "data": log_json(&row, Some(content)) }))
}

async fn read_content(state: &AppState, file_path: &str) -> ApiResult<Option<String>> {
    match mz_files::download_buffer(state, file_path).await {
        Ok(bytes) => Ok(Some(blocking(move || gunzip(&bytes)).await?)),
        // Satu object hilang jangan menggagalkan log lain di hari yang sama.
        Err(GcsError::NotFound) => {
            tracing::error!(file_path, "Object log tidak ditemukan");
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

/// `GET /users/log?date=`.
pub async fn get_logs(state: &AppState, user_id: i32, date: Option<&str>) -> ApiResult<Value> {
    let date = parse_log_date(date)?;
    let rows = sqlx::query_as::<_, LogRow>(&format!(
        r#"SELECT {COLUMNS} FROM "userLog" WHERE "userId" = $1 AND date = $2 ORDER BY "createdAt" ASC, id ASC"#
    ))
    .bind(user_id)
    .bind(date)
    .fetch_all(&state.db)
    .await?;

    let mut data = Vec::with_capacity(rows.len());
    for row in &rows {
        let content = match &row.file_path {
            Some(path) => read_content(state, path).await?,
            None => row.content.clone(),
        };
        data.push(log_json(row, content.as_deref()));
    }
    Ok(json!({ "success": true, "data": data }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_key_uses_utc_date() {
        let date = parse_log_date(Some("2024-01-15")).unwrap_or_default();
        assert_eq!(
            user_log_object_key(7, date, "abc"),
            "user-logs/7/2024-01-15/abc.log.gz"
        );
    }

    #[test]
    fn bad_date_is_400_with_node_message() {
        for bad in [None, Some("nope")] {
            assert!(matches!(
                parse_log_date(bad),
                Err(ApiError::Message { message, .. }) if message == "date must be a valid date (YYYY-MM-DD)"
            ));
        }
    }

    #[test]
    fn gzip_roundtrip() {
        let text = "baris 1\nbaris 2 \u{1F600}";
        let packed = gzip(text).unwrap_or_default();
        assert!(packed.len() > 2 && packed.first() == Some(&0x1f));
        assert_eq!(gunzip(&packed).ok().as_deref(), Some(text));
    }

    #[test]
    fn log_json_omits_file_path_and_uses_given_content() {
        let row = LogRow {
            id: 1,
            uuid: "u".into(),
            user_id: 2,
            date: JsDate::now(),
            content: Some("old".into()),
            file_path: Some("k".into()),
            created_at: JsDate::now(),
            updated_at: None,
        };
        let v = log_json(&row, Some("full text"));
        assert!(v.get("filePath").is_none());
        assert_eq!(v["content"], json!("full text"));
        assert_eq!(log_json(&row, None)["content"], Value::Null);
    }
}
