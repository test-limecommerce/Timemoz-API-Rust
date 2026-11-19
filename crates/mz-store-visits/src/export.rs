//! Bagian bersama export toko & kunjungan (dipakai juga `mz-orders`):
//! - `export-limits.const.ts` ([`CSV_EXPORT_MAX_ROWS`], [`XLSX_EXPORT_MAX_ROWS`], [`CSV_UTF8_BOM`]);
//! - `enums/export-format.enum.ts` ([`ExportFormat`], re-export dari [`crate::enums`]);
//! - `export-quota.util.ts` ([`consume_export_quota`]);
//! - limiter `main.ts` `exportLimiter` (5/jam per COMPANY, `rl:export:company`) → [`export_limiter`];
//! - sel/CSV/XLSX bersama kedua generator (`escapeCsvField`, `toCsvLine`, gaya header `write-excel-file`).

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use chrono::{Datelike, NaiveDate, NaiveDateTime};
use mz_core::rate_limit::{self, Outcome, RateKey, RateRule};
use mz_core::time::{JsDate, now_naive};
use mz_core::tz::zone_or_utc;
use mz_core::{ApiError, ApiResult, AppState};
use rust_xlsxwriter::{Color, Format, Workbook, XlsxError};
use serde_json::json;
use sqlx::PgPool;

pub use crate::enums::ExportFormat;

/// §7.6 — csv di-stream baris per baris, plafonnya longgar.
pub const CSV_EXPORT_MAX_ROWS: i64 = 50_000;
/// §7.6 — xlsx dibangun utuh di memori, plafonnya ketat.
pub const XLSX_EXPORT_MAX_ROWS: i64 = 5_000;
/// Tanpa BOM Excel membuka csv dalam codepage ANSI dan merusak karakter non-ASCII.
pub const CSV_UTF8_BOM: &str = "\u{feff}";

/// Plafon baris untuk format tertentu.
pub fn max_rows_for(format: ExportFormat) -> i64 {
    match format {
        ExportFormat::csv => CSV_EXPORT_MAX_ROWS,
        ExportFormat::xlsx => XLSX_EXPORT_MAX_ROWS,
    }
}

// ───────────────────────── kuota (export-quota.util.ts) ─────────────────────────

const MAX_CAS_ATTEMPTS: usize = 3;

/// `d.setMonth(d.getMonth() + 1)` (zona proses = UTC): tanggal dipertahankan dan meluap
/// (31 Jan → 3 Mar), jam tetap.
pub fn js_add_one_month(d: NaiveDateTime) -> NaiveDateTime {
    let (y, m) = if d.month() == 12 {
        (d.year() + 1, 1)
    } else {
        (d.year(), d.month() + 1)
    };
    let first = NaiveDate::from_ymd_opt(y, m, 1).unwrap_or(d.date());
    let date = first
        .checked_add_signed(chrono::Duration::days(i64::from(d.day()) - 1))
        .unwrap_or(first);
    date.and_time(d.time())
}

/// Hasil keputusan kuota murni (dipisah dari I/O supaya bisa diuji).
#[derive(Debug, Clone, PartialEq)]
pub enum QuotaDecision {
    /// Tulis `used`/`reset_at` baru dengan penjaga nilai lama.
    Consume { used: i32, reset_at: NaiveDateTime },
    /// 402 `EXPORT_QUOTA_EXCEEDED`.
    Exceeded { limit: i32, reset_at: NaiveDateTime },
}

/// Inti `consumeExportQuota` untuk satu percobaan.
pub fn decide_quota(
    per_month: i32,
    used: i32,
    reset_at: Option<NaiveDateTime>,
    now: NaiveDateTime,
) -> QuotaDecision {
    let (effective_used, effective_reset) = match reset_at {
        Some(r) if now <= r => (used, r),
        _ => (0, js_add_one_month(now)),
    };
    // -1 = tanpa batas.
    if per_month != -1 && effective_used >= per_month {
        return QuotaDecision::Exceeded {
            limit: per_month,
            reset_at: effective_reset,
        };
    }
    QuotaDecision::Consume {
        used: effective_used + 1,
        reset_at: effective_reset,
    }
}

/// `consumeExportQuota(prisma, companyId)` — kuota `company.reportExportsPerMonth` yang sama
/// dengan export laporan, dengan CAS optimistik (`updateMany` dijaga nilai yang baru dibaca,
/// 3 percobaan). Galat: 403 `No company found for user`, 402 `EXPORT_QUOTA_EXCEEDED`
/// (`details.resetAt` ms), 409 `EXPORT_QUOTA_CONFLICT`.
pub async fn consume_export_quota(db: &PgPool, company_id: i32) -> ApiResult<()> {
    for _ in 0..MAX_CAS_ATTEMPTS {
        let row: Option<(i32, i32, Option<NaiveDateTime>)> = sqlx::query_as(
            r#"SELECT "reportExportsPerMonth", "reportExportsUsedThisMonth", "reportExportsResetAt"
                 FROM company WHERE id = $1"#,
        )
        .bind(company_id)
        .fetch_optional(db)
        .await?;
        let Some((per_month, prior_used, prior_reset)) = row else {
            return Err(ApiError::forbidden("No company found for user"));
        };
        match decide_quota(per_month, prior_used, prior_reset, now_naive()) {
            QuotaDecision::Exceeded { limit, reset_at } => {
                return Err(ApiError::body(
                    StatusCode::PAYMENT_REQUIRED,
                    json!({
                        "code": "EXPORT_QUOTA_EXCEEDED",
                        "message": format!("Export quota exceeded. You have {limit} exports per month."),
                        "details": { "resetAt": reset_at.and_utc().timestamp_millis() },
                    }),
                ));
            }
            QuotaDecision::Consume { used, reset_at } => {
                // `reportExportsResetAt: null` di where Prisma = `IS NULL`.
                let n = sqlx::query(
                    r#"UPDATE company SET "reportExportsUsedThisMonth" = $2, "reportExportsResetAt" = $3
                        WHERE id = $1 AND "reportExportsUsedThisMonth" = $4
                          AND "reportExportsResetAt" IS NOT DISTINCT FROM $5"#,
                )
                .bind(company_id)
                .bind(used)
                .bind(reset_at)
                .bind(prior_used)
                .bind(prior_reset)
                .execute(db)
                .await?
                .rows_affected();
                if n > 0 {
                    return Ok(());
                }
            }
        }
    }
    Err(ApiError::body(
        StatusCode::CONFLICT,
        json!({
            "code": "EXPORT_QUOTA_CONFLICT",
            "message": "Could not update export quota due to concurrent exports, please retry",
        }),
    ))
}

// ───────────────────────── limiter per company (main.ts exportLimiter) ─────────────────────────

/// `exportLimiter`: 5 per jam per company, `keyPrefix: 'rl:export:company'`.
pub const EXPORT_LIMIT: RateRule = RateRule {
    key_prefix: "rl:export:company",
    max: Some(5),
    window_ms: 60 * 60 * 1000,
    key: RateKey::Token,
    pass_on_store_error: false,
};

/// `decodeJwtSubUnsafe(token)`: payload JWT TANPA verifikasi, hanya untuk memilih bucket.
/// `sub` harus `number` (bukan string).
pub fn decode_jwt_sub_unsafe(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    if payload.is_empty() {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let sub = v.get("sub")?;
    if let Some(i) = sub.as_i64() {
        return Some(i);
    }
    // `typeof sub === 'number'` pecahan: Prisma menolak nilai non-Int → tanpa company.
    sub.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)
}

/// Middleware `exportLimiter` (pasang dengan `axum::middleware::from_fn_with_state` di
/// LUAR `guard(..)`, jadi berjalan sebelum `JwtAuthGuard` seperti middleware express).
/// Tanpa bearer / `sub` / company → dilewati (`resolveKey` mengembalikan `null`).
/// `findCompanyId` gagal (DB error, `sub` di luar rentang Int → `PrismaClientValidationError`)
/// → 500 seperti Node (limiter tidak boleh terbuka diam-diam).
pub async fn export_limiter(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let key = match mz_core::jwt::extract_bearer(req.headers()).and_then(decode_jwt_sub_unsafe) {
        Some(sub) => {
            let found = match i32::try_from(sub) {
                Ok(uid) => mz_core::company::find_company_id(&state.db, uid)
                    .await
                    .map_err(|e| e.to_string()),
                Err(_) => Err(format!("sub di luar rentang Int: {sub}")),
            };
            match found {
                Ok(company) => company.map(|c| c.to_string()),
                Err(err) => {
                    tracing::error!(error = %err, "exportLimiter resolveKey gagal");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(
                            json!({ "statusCode": 500, "message": "Internal server error" }),
                        ),
                    )
                        .into_response();
                }
            }
        }
        None => None,
    };
    match rate_limit::check(&state, &EXPORT_LIMIT, key).await {
        Outcome::Limited(resp) => resp,
        Outcome::Pass(headers) => {
            let mut resp = next.run(req).await;
            if let Some(h) = headers {
                h.apply(resp.headers_mut());
            }
            resp
        }
    }
}

// ───────────────────────── sel, CSV, XLSX ─────────────────────────

/// Satu sel export (`{ value, kind }` Node). `Text("")` = `BLANK`/`s(null)`.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Text(String),
    Num(f64),
}

impl Cell {
    /// `s(value)`: `null`/`undefined` → teks kosong.
    pub fn s(v: Option<impl Into<String>>) -> Self {
        Self::Text(v.map(Into::into).unwrap_or_default())
    }
    /// `n(value)`: `null` → teks kosong.
    pub fn n(v: Option<f64>) -> Self {
        v.map_or(Self::Text(String::new()), Self::Num)
    }
    /// `BLANK`.
    pub fn blank() -> Self {
        Self::Text(String::new())
    }
    /// `String(value)` JS (angka dengan format `Number.prototype.toString`).
    pub fn to_js_string(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Num(n) => mz_core::jsfmt::num_string(*n),
        }
    }
}

/// `escapeCsvField`: dikutip bila memuat `"`, `,`, CR, atau LF.
pub fn escape_csv_field(value: &str) -> String {
    if value.contains(['"', ',', '\r', '\n']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

/// `toCsvLine`: sel dipisah koma, diakhiri CRLF.
pub fn csv_line(cells: &[Cell]) -> String {
    let mut s = cells
        .iter()
        .map(|c| escape_csv_field(&c.to_js_string()))
        .collect::<Vec<_>>()
        .join(",");
    s.push_str("\r\n");
    s
}

/// Baris judul CSV (diawali BOM).
pub fn csv_header_line(headers: &[&str]) -> String {
    let cells: Vec<Cell> = headers
        .iter()
        .map(|h| Cell::Text((*h).to_owned()))
        .collect();
    format!("{CSV_UTF8_BOM}{}", csv_line(&cells))
}

/// `write-excel-file`: satu sheet, judul tebal latar `#1a56db` huruf putih, sel angka/teks.
pub fn build_xlsx(sheet: &str, headers: &[&str], rows: &[Vec<Cell>]) -> Result<Vec<u8>, XlsxError> {
    let header = Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0x001a_56db))
        .set_font_color(Color::RGB(0x00ff_ffff));
    let mut wb = Workbook::new();
    let ws = wb.add_worksheet().set_name(sheet)?;
    for (ci, h) in headers.iter().enumerate() {
        ws.write_string_with_format(0, u16::try_from(ci).unwrap_or(u16::MAX), *h, &header)?;
    }
    for (ri, row) in rows.iter().enumerate() {
        let r = u32::try_from(ri + 1).unwrap_or(u32::MAX);
        for (ci, c) in row.iter().enumerate() {
            let col = u16::try_from(ci).unwrap_or(u16::MAX);
            match c {
                Cell::Text(s) => {
                    ws.write_string(r, col, s.chars().take(32_767).collect::<String>())?
                }
                Cell::Num(n) => ws.write_number(r, col, *n)?,
            };
        }
    }
    wb.save_to_buffer()
}

/// `moment(d).tz(timeZone).format('YYYY-MM-DD HH:mm:ss z')`; `None` → `''`.
pub fn format_date_tz(d: Option<JsDate>, time_zone: &str) -> String {
    d.map_or_else(String::new, |d| {
        d.utc()
            .with_timezone(&zone_or_utc(Some(time_zone)))
            .format("%Y-%m-%d %H:%M:%S %Z")
            .to_string()
    })
}

/// `Content-Type` export.
pub fn content_type_for(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::csv => "text/csv; charset=utf-8",
        ExportFormat::xlsx => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    }
}

/// Respons file utuh (`res.set(..); res.send(buffer)`), `Content-Disposition: attachment`.
pub fn file_response(content_type: &str, filename: &str, body: Vec<u8>) -> Response {
    let mut resp = (StatusCode::OK, body).into_response();
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(content_type) {
        h.insert(header::CONTENT_TYPE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    #[test]
    fn set_month_overflows_like_js() {
        assert_eq!(
            js_add_one_month(dt("2026-01-31 10:00:00")),
            dt("2026-03-03 10:00:00")
        );
        assert_eq!(
            js_add_one_month(dt("2026-12-15 00:00:00")),
            dt("2027-01-15 00:00:00")
        );
    }

    #[test]
    fn quota_under_limit_increments() {
        let now = dt("2026-05-10 00:00:00");
        let reset = dt("2026-06-01 00:00:00");
        assert_eq!(
            decide_quota(3, 1, Some(reset), now),
            QuotaDecision::Consume {
                used: 2,
                reset_at: reset
            }
        );
    }

    #[test]
    fn quota_resets_after_reset_at() {
        let now = dt("2026-05-10 00:00:00");
        let d = decide_quota(3, 3, Some(dt("2026-05-01 00:00:00")), now);
        assert_eq!(
            d,
            QuotaDecision::Consume {
                used: 1,
                reset_at: dt("2026-06-10 00:00:00")
            }
        );
        assert!(matches!(
            decide_quota(3, 9, None, now),
            QuotaDecision::Consume { used: 1, .. }
        ));
    }

    #[test]
    fn quota_unlimited_and_exceeded() {
        let now = dt("2026-05-10 00:00:00");
        let reset = dt("2026-06-01 00:00:00");
        assert!(matches!(
            decide_quota(-1, 999, Some(reset), now),
            QuotaDecision::Consume { used: 1000, .. }
        ));
        assert_eq!(
            decide_quota(3, 3, Some(reset), now),
            QuotaDecision::Exceeded {
                limit: 3,
                reset_at: reset
            }
        );
    }

    #[test]
    fn csv_escaping() {
        assert_eq!(escape_csv_field("plain"), "plain");
        assert_eq!(escape_csv_field("a,b"), "\"a,b\"");
        assert_eq!(escape_csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(
            csv_line(&[Cell::Text("x".into()), Cell::Num(1.5), Cell::blank()]),
            "x,1.5,\r\n"
        );
        assert!(csv_header_line(&["id"]).starts_with('\u{feff}'));
    }

    #[test]
    fn jwt_sub_decoding() {
        let enc = |v: serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string())
        };
        let t = format!("h.{}.s", enc(json!({ "sub": 42 })));
        assert_eq!(decode_jwt_sub_unsafe(&t), Some(42));
        let t = format!("h.{}.s", enc(json!({ "sub": "42" })));
        assert_eq!(decode_jwt_sub_unsafe(&t), None);
        assert_eq!(decode_jwt_sub_unsafe("garbage"), None);
    }

    #[test]
    fn dates_in_company_zone() {
        let d = JsDate::from_millis(1_767_322_800_000);
        assert_eq!(format_date_tz(d, "Asia/Jakarta"), "2026-01-02 10:00:00 WIB");
        assert_eq!(format_date_tz(None, "UTC"), "");
    }
}
