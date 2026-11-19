//! Pembantu bersama crate: galat ber-`code`, serialisasi waktu epoch-ms, desimal ala Prisma,
//! meta paginasi, dan pembacaan company (`timeZone`/`currency`).

use chrono::NaiveDateTime;
use http::StatusCode;
use mz_core::{ApiError, ApiResult};
use rust_decimal::Decimal;
use serde_json::{Value, json};

/// `new XxxException({ code, message })`.
pub fn coded(status: StatusCode, code: &str, message: impl Into<String>) -> ApiError {
    ApiError::body(status, json!({ "code": code, "message": message.into() }))
}

/// `new XxxException({ code, message, details })`.
pub fn coded_details(
    status: StatusCode,
    code: &str,
    message: impl Into<String>,
    details: Value,
) -> ApiError {
    ApiError::body(
        status,
        json!({ "code": code, "message": message.into(), "details": details }),
    )
}

pub fn not_found(code: &str, message: impl Into<String>) -> ApiError {
    coded(StatusCode::NOT_FOUND, code, message)
}

pub fn unprocessable(code: &str, message: impl Into<String>) -> ApiError {
    coded(StatusCode::UNPROCESSABLE_ENTITY, code, message)
}

pub fn conflict(code: &str, message: impl Into<String>) -> ApiError {
    coded(StatusCode::CONFLICT, code, message)
}

pub fn bad_request(code: &str, message: impl Into<String>) -> ApiError {
    coded(StatusCode::BAD_REQUEST, code, message)
}

/// 500 (galat Prisma/TypeError yang tidak ditangkap Node).
pub fn internal(msg: &str) -> ApiError {
    ApiError::internal(anyhow::anyhow!("{msg}"))
}

/// Kolom `@db.Uuid` dicari dengan string non-UUID: Prisma P2023 → 500 di Node.
pub fn row_uuid(raw: &str) -> ApiResult<uuid::Uuid> {
    mz_core::prisma::parse_uuid(raw).map_err(|m| internal(&m))
}

/// `date.getTime()` untuk kolom `DateTime` Prisma (timestamp tanpa zona = UTC).
pub fn ms(dt: NaiveDateTime) -> i64 {
    dt.and_utc().timestamp_millis()
}

/// `x ? x.getTime() : null`.
pub fn ms_opt(dt: Option<NaiveDateTime>) -> Value {
    dt.map_or(Value::Null, |d| Value::from(ms(d)))
}

/// `String(prismaDecimal)` — decimal.js membuang nol di belakang (`"24.000000"` → `"24"`).
pub fn dec_str(d: &Decimal) -> String {
    d.normalize().to_string()
}

pub fn dec_opt(d: Option<&Decimal>) -> Value {
    d.map_or(Value::Null, |d| Value::String(dec_str(d)))
}

/// `{ total, page, limit, totalPages: Math.ceil(total / limit) }`.
pub fn page_meta(total: i64, page: i64, limit: i64) -> Value {
    let total_pages = if limit > 0 {
        (total + limit - 1).div_euclid(limit)
    } else {
        0
    };
    json!({ "total": total, "page": page, "limit": limit, "totalPages": total_pages })
}

/// `(page - 1) * limit`.
pub fn offset(page: i64, limit: i64) -> i64 {
    page.saturating_sub(1).saturating_mul(limit)
}

/// `{ data }` + status.
pub fn data(v: Value) -> axum::Json<Value> {
    axum::Json(json!({ "data": v }))
}

/// `company.timeZone || 'UTC'` (null company → `None`).
pub async fn company_time_zone(db: &sqlx::PgPool, company_id: i32) -> ApiResult<Option<String>> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as(r#"SELECT "timeZone" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(db)
            .await?;
    Ok(row.map(|(tz,)| {
        tz.filter(|t| !t.is_empty())
            .unwrap_or_else(|| "UTC".to_owned())
    }))
}

/// Konstraint unik yang dilanggar (nama index), bila galatnya unique violation.
pub fn unique_constraint(err: &sqlx::Error) -> Option<String> {
    match err {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            Some(db.constraint().unwrap_or_default().to_owned())
        }
        _ => None,
    }
}

/// `escape` untuk `ILIKE` (`contains` Prisma memakai `%x%` dengan `\`, `%`, `_` di-escape).
pub fn like_pattern(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('%');
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn decimal_seperti_prisma() {
        assert_eq!(dec_str(&Decimal::from_str("24.000000").unwrap()), "24");
        assert_eq!(dec_str(&Decimal::from_str("0.001000").unwrap()), "0.001");
        assert_eq!(dec_str(&Decimal::from_str("12500.00").unwrap()), "12500");
        assert_eq!(dec_str(&Decimal::from_str("0.000000").unwrap()), "0");
    }

    #[test]
    fn meta_halaman() {
        assert_eq!(page_meta(0, 1, 20)["totalPages"], 0);
        assert_eq!(page_meta(21, 2, 20)["totalPages"], 2);
        assert_eq!(offset(3, 20), 40);
    }

    #[test]
    fn pola_like() {
        assert_eq!(like_pattern("a_b%"), "%a\\_b\\%%");
    }
}
