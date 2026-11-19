//! `POST /admin/screenshot/compress|purge`, `DELETE /admin/screenshot/queue`,
//! `POST /admin/screenshot/queue/pause|resume`
//! (`src/admin/admin.controller.ts` `manualCompressScreenshots` / `manualPurgeScreenshots` /
//! `clearScreenshotQueue` / `pauseScreenshotQueue` / `resumeScreenshotQueue`).
//!
//! Controller admin: `@PermissionExempt('separate-auth')` + `JwtAdminAuthGuard` (extractor
//! [`Operator`]), `@UsePipes(ADMIN_VALIDATION_PIPE)` (`whitelist` + `forbidNonWhitelisted`),
//! `OperatorAuditInterceptor` + `@OperatorAuditAction('screenshot.*', { targetType: 'screenshot' })`
//! (baris `operatorAuditLog` sesudah sukses), `@HttpCode(200)`.
//!
//! Node mengantrekan job Bull `screenshot.compress|purge` (diproses `ScreenshotConsumer`); di sini
//! job yang sama masuk antrean persisten `mz_core::queue` bernama
//! [`mz_screenshots::jobs::QUEUE`] (Redis) dan dikerjakan [`mz_screenshots::jobs::spawn_worker`].
//! Lihat `plan/deviations/mz-timeconfig.md` bagian "Admin screenshot".

use axum::Json;
use axum::extract::State;
use axum::http::request::Parts;
use chrono::{Duration as ChronoDuration, NaiveDateTime, Timelike};
use mz_core::extract::check_int_range as check_query_int;
use mz_core::jsfmt::{number as js_number, trim as js_trim};
use mz_core::qs::QVal;
use mz_core::queue;
use mz_core::{ApiError, ApiResult, AppState};
use mz_screenshots::jobs::{QUEUE, enqueue_compress, enqueue_purge};
use serde::Serialize;
use serde_json::{Value, json};

use crate::operator::{AuditCtx, Operator, record_action_for};

/// `take: 5000` di kedua handler.
const TAKE: i64 = 5000;
const TARGET_TYPE: &str = "screenshot";

/// Respons handler (urutan kunci = literal objek Node).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobResponse {
    pub success: bool,
    pub message: String,
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days_old: Option<i64>,
}

// ── Validasi (`ADMIN_VALIDATION_PIPE`) ──────────────────────────────────────

/// `ScreenshotDaysOldDto` (`@IsOptional @Type(() => Number) @IsInt @Min(1) @Max(3650)`) di
/// bawah `whitelist + forbidNonWhitelisted`: kunci lain → `property <k> should not exist`
/// (lebih dulu, urutan kemunculan), lalu pesan Max, Min, IsInt.
pub fn validate_days_old(raw_query: Option<&str>) -> Result<Option<i64>, ApiError> {
    let map = mz_core::qs::parse(raw_query.unwrap_or_default());
    let mut errors = Vec::new();
    for (k, _) in &map {
        if k != "daysOld" {
            errors.push(format!("property {k} should not exist"));
        }
    }
    let days = match map.iter().find(|(k, _)| k == "daysOld").map(|(_, v)| v) {
        None => None,
        Some(QVal::One(s)) => check_query_int(&mut errors, "daysOld", Some(js_number(s)), 1, 3650),
        Some(QVal::Many(_) | QVal::Obj) => check_query_int(&mut errors, "daysOld", None, 1, 3650),
    };
    if errors.is_empty() {
        Ok(days)
    } else {
        Err(ApiError::Validation(errors))
    }
}

/// `ReasonDto` (`@Transform(trim) @IsString @Length(5, 500)`) di bawah `whitelist +
/// forbidNonWhitelisted`. Urutan pesan: properti asing, Length, IsString (diverifikasi
/// terhadap `ValidationPipe` Node). Body array → `property <indeks> should not exist`.
pub fn validate_reason(body: &Value) -> Result<(), ApiError> {
    let mut errors = Vec::new();
    let reason = match body {
        Value::Object(m) => {
            for k in m.keys() {
                if k != "reason" {
                    errors.push(format!("property {k} should not exist"));
                }
            }
            m.get("reason")
        }
        Value::Array(a) => {
            for i in 0..a.len() {
                errors.push(format!("property {i} should not exist"));
            }
            None
        }
        _ => None,
    };
    let trimmed = reason.map(|v| match v {
        Value::String(s) => Value::String(js_trim(s).to_owned()),
        other => other.clone(),
    });
    if let Some(msg) = mz_core::extract::length_error("reason", trimmed.as_ref(), 5, 500) {
        errors.push(msg);
    }
    if !matches!(trimmed, Some(Value::String(_))) {
        errors.push("reason must be a string".to_owned());
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ApiError::Validation(errors))
    }
}

/// Body mentah ala `express.json` (strict) — satu implementasi di `mz_core::extract`.
pub use mz_core::extract::RawBody;

fn raw_query(parts: &Parts) -> Option<&str> {
    parts.uri.query()
}

/// Query mentah (extractor tanpa gagal).
pub struct RawQuery(pub Option<String>);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for RawQuery {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(raw_query(parts).map(str::to_owned)))
    }
}

// ── Jendela waktu ───────────────────────────────────────────────────────────

/// Jendela `manualCompressScreenshots`: `[tengah malam WIB hari ini − daysOld hari, +1 hari)`
/// dalam UTC. Node menghitungnya lewat `toLocaleString('Asia/Jakarta')` yang diparse ulang
/// sebagai waktu lokal proses; setara hanya bila proses Node ber-TZ UTC (bawaan container).
pub fn compress_window(now_utc: NaiveDateTime, days_old: i64) -> (NaiveDateTime, NaiveDateTime) {
    let wib = now_utc + ChronoDuration::hours(7);
    let midnight_wib = wib
        .with_hour(0)
        .and_then(|t| t.with_minute(0))
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(wib);
    let today_midnight_utc = midnight_wib - ChronoDuration::hours(7);
    let from = today_midnight_utc - ChronoDuration::days(days_old);
    (from, from + ChronoDuration::days(1))
}

// ── Handler ─────────────────────────────────────────────────────────────────

/// Node: src/admin/admin.controller.ts:421 (`manualCompressScreenshots`).
pub async fn h_compress(
    State(state): State<AppState>,
    op: Operator,
    audit: AuditCtx,
    RawQuery(q): RawQuery,
    RawBody(body): RawBody,
) -> ApiResult<Json<JobResponse>> {
    let days = validate_days_old(q.as_deref())?.unwrap_or(7);
    let (from, to) = compress_window(mz_core::time::now_naive(), days);
    let ids: Vec<i32> = sqlx::query_scalar(
        r#"SELECT s.id FROM screenshot s
            WHERE s."isCompressed" = false AND s."createdAt" >= $1 AND s."createdAt" < $2
              AND EXISTS (SELECT 1 FROM "projectCompany" pc JOIN company c ON c.id = pc."companyId"
                           WHERE pc."projectId" = s."projectId" AND c."screenshotRetentionDays" >= 0)
            ORDER BY s.id LIMIT $3"#,
    )
    .bind(from)
    .bind(to)
    .bind(TAKE)
    .fetch_all(&state.db)
    .await?;
    let res = if ids.is_empty() {
        JobResponse {
            success: true,
            message: format!("No screenshots found older than {days} days to compress"),
            count: 0,
            days_old: None,
        }
    } else {
        let n = ids.len();
        enqueue_compress(&state, &ids)
            .await
            .map_err(ApiError::internal)?;
        JobResponse {
            success: true,
            message: format!("Enqueued {n} screenshots for compression"),
            count: n,
            days_old: Some(days),
        }
    };
    record_action_for(
        &state,
        &op,
        &audit,
        TARGET_TYPE,
        "screenshot.compress",
        None,
        Some(&body),
    )
    .await;
    Ok(Json(res))
}

/// Node: src/admin/admin.controller.ts:503 (`manualPurgeScreenshots`).
pub async fn h_purge(
    State(state): State<AppState>,
    op: Operator,
    audit: AuditCtx,
    RawQuery(q): RawQuery,
    RawBody(body): RawBody,
) -> ApiResult<Json<JobResponse>> {
    let days = validate_days_old(q.as_deref())?.unwrap_or(30);
    validate_reason(&body)?;
    let cutoff = mz_core::time::now_naive() - ChronoDuration::days(days);
    let ids: Vec<i32> = sqlx::query_scalar(
        r#"SELECT s.id FROM screenshot s
            WHERE s."createdAt" < $1
              AND EXISTS (SELECT 1 FROM "projectCompany" pc JOIN company c ON c.id = pc."companyId"
                           WHERE pc."projectId" = s."projectId" AND c."screenshotRetentionDays" <= $2)
            ORDER BY s.id LIMIT $3"#,
    )
    .bind(cutoff)
    .bind(i32::try_from(days).unwrap_or(i32::MAX))
    .bind(TAKE)
    .fetch_all(&state.db)
    .await?;
    let res = if ids.is_empty() {
        JobResponse {
            success: true,
            message: format!("No screenshots found older than {days} days to purge"),
            count: 0,
            days_old: None,
        }
    } else {
        let n = ids.len();
        enqueue_purge(&state, &ids)
            .await
            .map_err(ApiError::internal)?;
        JobResponse {
            success: true,
            message: format!("Enqueued {n} screenshots for purge"),
            count: n,
            days_old: Some(days),
        }
    };
    record_action_for(
        &state,
        &op,
        &audit,
        TARGET_TYPE,
        "screenshot.purge",
        None,
        Some(&body),
    )
    .await;
    Ok(Json(res))
}

/// Node: src/admin/admin.controller.ts:579 (`clearScreenshotQueue`). `@Body() ReasonDto` saja
/// yang divalidasi (query tidak punya DTO, jadi diabaikan).
///
/// `removed.failed` selalu 0: antrean tidak menyimpan job gagal (`attempts: 1` + dibuang sesudah
/// log; Node `removeOnFail: true` membuat `getFailed()` kosong juga).
pub async fn h_queue_clear(
    State(state): State<AppState>,
    op: Operator,
    audit: AuditCtx,
    RawBody(body): RawBody,
) -> ApiResult<Json<Value>> {
    validate_reason(&body)?;
    let waiting = queue::clear_waiting(&state, QUEUE)
        .await
        .map_err(ApiError::internal)?;
    record_action_for(
        &state,
        &op,
        &audit,
        TARGET_TYPE,
        "screenshot.queue_clear",
        None,
        Some(&body),
    )
    .await;
    Ok(Json(json!({
        "success": true,
        "message": "Screenshot queue cleared",
        "removed": { "waiting": waiting, "failed": 0 }
    })))
}

/// Node: src/admin/admin.controller.ts:608 (`pauseScreenshotQueue`). Tanpa `@Body`/`@Query`:
/// body apa pun diterima (hanya JSON rusak yang 400, seperti body-parser).
pub async fn h_queue_pause(
    State(state): State<AppState>,
    op: Operator,
    audit: AuditCtx,
    RawBody(body): RawBody,
) -> ApiResult<Json<Value>> {
    queue::pause(&state, QUEUE)
        .await
        .map_err(ApiError::internal)?;
    record_action_for(
        &state,
        &op,
        &audit,
        TARGET_TYPE,
        "screenshot.queue_pause",
        None,
        Some(&body),
    )
    .await;
    Ok(Json(
        json!({ "success": true, "message": "Screenshot queue paused" }),
    ))
}

/// Node: src/admin/admin.controller.ts:620 (`resumeScreenshotQueue`).
pub async fn h_queue_resume(
    State(state): State<AppState>,
    op: Operator,
    audit: AuditCtx,
    RawBody(body): RawBody,
) -> ApiResult<Json<Value>> {
    queue::resume(&state, QUEUE)
        .await
        .map_err(ApiError::internal)?;
    record_action_for(
        &state,
        &op,
        &audit,
        TARGET_TYPE,
        "screenshot.queue_resume",
        None,
        Some(&body),
    )
    .await;
    Ok(Json(
        json!({ "success": true, "message": "Screenshot queue resumed" }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msgs(r: Result<impl std::fmt::Debug, ApiError>) -> Vec<String> {
        match r {
            Err(ApiError::Validation(m)) => m,
            other => vec![format!("{other:?}")],
        }
    }

    /// Hasil `ValidationPipe` Node 10.4.22 (dijalankan langsung terhadap `dist` DTO Node).
    #[test]
    fn days_old_matches_node_pipe() {
        assert_eq!(validate_days_old(None).ok(), Some(None));
        assert_eq!(validate_days_old(Some("daysOld=7")).ok(), Some(Some(7)));
        assert_eq!(validate_days_old(Some("daysOld=1e1")).ok(), Some(Some(10)));
        assert_eq!(validate_days_old(Some("daysOld=0x10")).ok(), Some(Some(16)));
        assert_eq!(
            validate_days_old(Some("daysOld=%207%20")).ok(),
            Some(Some(7))
        );
        let all3 = vec![
            "daysOld must not be greater than 3650",
            "daysOld must not be less than 1",
            "daysOld must be an integer number",
        ];
        assert_eq!(msgs(validate_days_old(Some("daysOld=abc"))), all3);
        assert_eq!(msgs(validate_days_old(Some("daysOld=1&daysOld=2"))), all3);
        assert_eq!(
            msgs(validate_days_old(Some("daysOld="))),
            vec!["daysOld must not be less than 1"]
        );
        assert_eq!(
            msgs(validate_days_old(Some("daysOld=0"))),
            vec!["daysOld must not be less than 1"]
        );
        assert_eq!(
            msgs(validate_days_old(Some("daysOld=1.5"))),
            vec!["daysOld must be an integer number"]
        );
        assert_eq!(
            msgs(validate_days_old(Some("daysOld=3651"))),
            vec!["daysOld must not be greater than 3650"]
        );
        assert_eq!(
            msgs(validate_days_old(Some("daysOld=7&foo=x&bar=y"))),
            vec![
                "property foo should not exist",
                "property bar should not exist"
            ]
        );
        let mut extra_bad = vec!["property foo should not exist"];
        extra_bad.extend(all3);
        assert_eq!(
            msgs(validate_days_old(Some("foo=x&daysOld=abc"))),
            extra_bad
        );
    }

    #[test]
    fn reason_matches_node_pipe() {
        let longer = "reason must be longer than or equal to 5 characters";
        let not_string = "reason must be a string";
        assert_eq!(msgs(validate_reason(&json!({}))), vec![longer, not_string]);
        assert_eq!(
            msgs(validate_reason(&json!({ "reason": "  abc  " }))),
            vec![longer]
        );
        assert!(validate_reason(&json!({ "reason": "  valid reason  " })).is_ok());
        assert_eq!(
            msgs(validate_reason(&json!({ "reason": 12345 }))),
            vec![
                "reason must be longer than or equal to 5 and shorter than or equal to 500 characters",
                not_string
            ]
        );
        assert_eq!(
            msgs(validate_reason(
                &json!({ "reason": "valid reason", "x": 1 })
            )),
            vec!["property x should not exist"]
        );
        assert_eq!(
            msgs(validate_reason(&json!(["valid reason"]))),
            vec!["property 0 should not exist", longer, not_string]
        );
        assert_eq!(
            msgs(validate_reason(&json!({ "reason": "x".repeat(501) }))),
            vec!["reason must be shorter than or equal to 500 characters"]
        );
        assert!(validate_reason(&json!({ "reason": "😀😀😀😀😀" })).is_ok());
        assert_eq!(
            msgs(validate_reason(&json!({ "reason": null }))),
            vec![longer, not_string]
        );
        // validator.js menghitung 4 (tidak valid) tetapi `value.length` UTF-16 = 8 → pesan gabungan.
        assert_eq!(
            msgs(validate_reason(&json!({ "reason": "😀😀😀😀" }))),
            vec![
                "reason must be longer than or equal to 5 and shorter than or equal to 500 characters"
            ]
        );
    }

    #[test]
    fn compress_window_is_wib_midnight_in_utc() {
        let at =
            |s: &str| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap_or_default();
        // 2026-11-16 18:00 UTC = 17 Nov 01:00 WIB → tengah malam WIB 17 Nov = 16 Nov 17:00 UTC.
        assert_eq!(
            compress_window(at("2026-11-16 18:00:00"), 7),
            (at("2026-11-09 17:00:00"), at("2026-11-10 17:00:00"))
        );
        // 2026-11-16 10:00 UTC = 17:00 WIB (hari yang sama).
        assert_eq!(
            compress_window(at("2026-11-16 10:00:00"), 1),
            (at("2026-11-14 17:00:00"), at("2026-11-15 17:00:00"))
        );
    }
}
