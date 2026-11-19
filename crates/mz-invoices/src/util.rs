//! Pembantu bersama: galat ala `try/catch` service Node, tanggal ala `Date` (zona proses = UTC
//! di produksi), dan pembacaan env ala template string JS.

use axum::http::StatusCode;
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime};
use mz_core::time::JsDate;
use mz_core::{ApiError, ApiResult, AppState};
use mz_paypal::PaypalError;
use serde_json::{Value, json};

/// Galat di dalam blok `try` service Node.
///
/// Semua method `InvoiceService` yang di-port menangkap galat dan mengembalikan
/// `{ success: false, message: error.message || '<fallback>' }`, KECUALI `ConflictException`
/// (`MANUAL_BILLING`) yang dilempar ulang.
#[derive(Debug)]
pub enum Fail {
    /// `if (error instanceof ConflictException) throw error;`
    Throw(ApiError),
    /// `error.message` yang bisa ditiru persis (axios PayPal/Midtrans, `TypeError` JS).
    Message(String),
    /// Galat lain (Prisma/DB). Teks galat Prisma (dengan cuplikan kode sumber) tidak bisa ditiru:
    /// dipakai pesan fallback method (lihat `plan/deviations/mz-invoices.md`).
    Other(String),
}

impl From<sqlx::Error> for Fail {
    fn from(e: sqlx::Error) -> Self {
        Self::Other(e.to_string())
    }
}

impl From<PaypalError> for Fail {
    fn from(e: PaypalError) -> Self {
        Self::Message(e.message)
    }
}

impl From<ApiError> for Fail {
    fn from(e: ApiError) -> Self {
        Self::Other(e.to_string())
    }
}

/// `TypeError` JS untuk `undefined.x` / `null.x`.
pub fn type_error(on: &str, prop: &str) -> Fail {
    Fail::Message(format!("Cannot read properties of {on} (reading '{prop}')"))
}

/// `catch (error) { if (error instanceof ConflictException) throw error; return { success:false,
/// message: error.message || fallback } }`.
pub fn finish(result: Result<Value, Fail>, fallback: &str, op: &str) -> ApiResult<Value> {
    match result {
        Ok(v) => Ok(v),
        Err(Fail::Throw(e)) => Err(e),
        Err(Fail::Message(m)) => Ok(json!({
            "success": false,
            "message": if m.is_empty() { fallback.to_owned() } else { m },
        })),
        Err(Fail::Other(detail)) => {
            tracing::error!(op, error = %detail, "invoices: galat");
            Ok(json!({ "success": false, "message": fallback }))
        }
    }
}

/// `assertNotManualBilling(prisma, companyId)` di dalam `try`: 409 dilempar ulang, galat DB
/// tertangkap seperti galat lain.
pub async fn assert_not_manual(state: &AppState, company_id: i32) -> Result<(), Fail> {
    match mz_billing::assert_not_manual_billing(&state.db, company_id).await {
        Ok(()) => Ok(()),
        Err(e) if e.status() == StatusCode::CONFLICT => Err(Fail::Throw(e)),
        Err(e) => Err(e.into()),
    }
}

/// `{ success: false, message }`.
pub fn fail_msg(message: &str) -> Value {
    json!({ "success": false, "message": message })
}

/// Nilai JSON tanggal (`Date.toJSON()`).
pub fn date_json(d: NaiveDateTime) -> Value {
    Value::String(JsDate(d).to_js_string())
}

/// `d.toISOString()`.
pub fn iso(d: NaiveDateTime) -> String {
    JsDate(d).to_js_string()
}

/// `DD-MM-YYYY` dari `getDate()/getMonth()/getFullYear()` (zona proses; produksi TZ=UTC).
pub fn dmy(d: NaiveDateTime) -> String {
    d.format("%d-%m-%Y").to_string()
}

/// `d.setDate(d.getDate() + n)` (UTC: tepat `n` hari).
pub fn add_days(d: NaiveDateTime, n: i64) -> NaiveDateTime {
    d + Duration::days(n)
}

/// `d.setFullYear(d.getFullYear() + 1)`: 29 Feb → 1 Mar (tanggal JS melimpah ke bulan berikutnya).
pub fn add_year_js(d: NaiveDateTime) -> NaiveDateTime {
    let date = d.date();
    let next = date
        .with_year(date.year() + 1)
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(date.year() + 1, 3, 1).unwrap_or(date));
    next.and_time(d.time())
}

/// `${process.env.X}` di template string: env kosong → `"undefined"`.
pub fn env_js(state: &AppState, key: &str) -> String {
    state
        .config
        .var(key)
        .unwrap_or_else(|| "undefined".to_owned())
}

/// `{ ...(x && {k: v}) }` / properti `undefined` yang dibuang `JSON.stringify`.
pub fn opt_str(v: Option<String>) -> Value {
    v.map_or(Value::Null, Value::String)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, day)
            .and_then(|x| x.and_hms_milli_opt(7, 8, 9, 10))
            .unwrap_or_default()
    }

    #[test]
    fn tanggal_js() {
        assert_eq!(dmy(d(2026, 1, 5)), "05-01-2026");
        assert_eq!(add_days(d(2026, 3, 2), -3), d(2026, 2, 27));
        assert_eq!(add_days(d(2026, 12, 30), 3), d(2027, 1, 2));
        assert_eq!(add_year_js(d(2028, 2, 29)), d(2029, 3, 1));
        assert_eq!(add_year_js(d(2026, 11, 17)), d(2027, 11, 17));
        assert_eq!(iso(d(2026, 11, 17)), "2026-11-17T07:08:09.010Z");
    }

    #[test]
    fn finish_meniru_catch_node() {
        let v = finish(Err(Fail::Message(String::new())), "Failed to x", "t").unwrap();
        assert_eq!(v, json!({"success": false, "message": "Failed to x"}));
        let v = finish(Err(Fail::Message("boom".into())), "Failed to x", "t").unwrap();
        assert_eq!(v["message"], "boom");
        let v = finish(Err(Fail::Other("db".into())), "Failed to x", "t").unwrap();
        assert_eq!(v["message"], "Failed to x");
        assert!(
            finish(
                Err(Fail::Throw(mz_billing::manual_billing_error())),
                "x",
                "t"
            )
            .is_err()
        );
        match type_error("undefined", "companyId") {
            Fail::Message(m) => {
                assert_eq!(
                    m,
                    "Cannot read properties of undefined (reading 'companyId')"
                );
            }
            other => panic!("{other:?}"),
        }
    }
}
