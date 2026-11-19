//! Port `src/overtime/overtime-errors.ts`.

use axum::http::StatusCode;
use mz_core::ApiError;
use serde_json::{Map, Value};

/// `overtimeError(status, code, message, details?)`: body `{ code, message, details? }`
/// (kunci `details` dihilangkan bila `None`), status apa adanya.
pub fn overtime_error(
    status: StatusCode,
    code: &str,
    message: impl Into<String>,
    details: Option<Value>,
) -> ApiError {
    let mut body = Map::new();
    body.insert("code".into(), Value::String(code.to_owned()));
    body.insert("message".into(), Value::String(message.into()));
    if let Some(details) = details {
        body.insert("details".into(), details);
    }
    ApiError::body(status, Value::Object(body))
}

/// Nama partial unique index `overtimeRequest ("companyId","userId","date")` (FR-019).
pub const DATE_TAKEN_INDEX: &str = "overtimeRequest_one_active_per_day";

/// Kolom unique parsial yang dilaporkan Prisma (`DATE_TAKEN_COLUMNS`).
const DATE_TAKEN_COLUMNS: &[&str] = &["companyId", "userId", "date"];

/// Murni: target unique (nama kolom, boleh berupa `"a,b,c"`) mencakup ketiga kolom
/// `companyId,userId,date` tanpa memperhatikan urutan.
pub fn is_date_taken_target(targets: &[&str]) -> bool {
    let cols: Vec<&str> = targets
        .iter()
        .flat_map(|t| t.split(','))
        .map(|t| t.trim().trim_matches('"'))
        .collect();
    DATE_TAKEN_COLUMNS.iter().all(|c| cols.contains(c))
}

/// Murni: apakah unique violation (nama constraint + `detail` Postgres) adalah index
/// "satu pengajuan aktif per hari". `detail` berbentuk
/// `Key ("companyId", "userId", date)=(…) already exists.`.
pub fn is_date_taken_violation(constraint: Option<&str>, detail: Option<&str>) -> bool {
    if constraint == Some(DATE_TAKEN_INDEX) {
        return true;
    }
    let Some(detail) = detail else { return false };
    let Some(cols) = detail
        .strip_prefix("Key (")
        .and_then(|rest| rest.split_once(")="))
        .map(|(cols, _)| cols)
    else {
        return false;
    };
    is_date_taken_target(&[cols])
}

/// `409 OVERTIME_DATE_TAKEN`.
pub fn date_taken_error() -> ApiError {
    overtime_error(
        StatusCode::CONFLICT,
        "OVERTIME_DATE_TAKEN",
        "Sudah ada pengajuan lembur aktif di tanggal ini.",
        None,
    )
}

/// Padanan `mapOvertimeP2002` (TOCTOU T1): unique violation pada partial index
/// `companyId,userId,date` → `409 OVERTIME_DATE_TAKEN`; galat lain diteruskan apa adanya
/// (`ApiError::from(sqlx::Error)`, termasuk unique violation lain → 409 `status:false`).
pub fn map_overtime_unique(err: sqlx::Error) -> ApiError {
    if let sqlx::Error::Database(db) = &err
        && db.is_unique_violation()
    {
        let detail = db
            .try_downcast_ref::<sqlx::postgres::PgDatabaseError>()
            .and_then(|pg| pg.detail());
        if is_date_taken_violation(db.constraint(), detail) {
            return date_taken_error();
        }
    }
    ApiError::from(err)
}

/// `404 OVERTIME_NOT_FOUND` (bunyi sama di service requests & approvals Node).
pub fn not_found_error() -> ApiError {
    overtime_error(
        StatusCode::NOT_FOUND,
        "OVERTIME_NOT_FOUND",
        "Pengajuan lembur tidak ditemukan.",
        None,
    )
}

/// T2: `409 OVERTIME_STATE_CONFLICT` (tanpa `details`; bunyi sama di requests & approvals Node).
pub fn state_conflict_error() -> ApiError {
    overtime_error(
        StatusCode::CONFLICT,
        "OVERTIME_STATE_CONFLICT",
        "Status pengajuan lembur sudah berubah. Muat ulang lalu coba lagi.",
        None,
    )
}

#[cfg(test)]
mod tests {
    //! Kasus `overtime-errors.spec.ts`.
    #![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use axum::response::IntoResponse;
    use serde_json::json;

    async fn body_of(err: ApiError) -> (u16, Value) {
        let res = err.into_response();
        let status = res.status().as_u16();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap_or_default();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn body_without_details_key() {
        let (status, body) = body_of(overtime_error(
            StatusCode::FORBIDDEN,
            "OVERTIME_DISABLED",
            "Nonaktif",
            None,
        ))
        .await;
        assert_eq!(status, 403);
        assert_eq!(
            body,
            json!({"code": "OVERTIME_DISABLED", "message": "Nonaktif"})
        );
        assert!(body.get("details").is_none());
    }

    #[tokio::test]
    async fn body_with_details() {
        let (status, body) = body_of(overtime_error(
            StatusCode::CONFLICT,
            "OVERTIME_STATE_CONFLICT",
            "Konflik",
            Some(json!({"current": "CANCELLED"})),
        ))
        .await;
        assert_eq!(status, 409);
        assert_eq!(
            body,
            json!({"code": "OVERTIME_STATE_CONFLICT", "message": "Konflik", "details": {"current": "CANCELLED"}})
        );
    }

    #[test]
    fn date_taken_targets() {
        assert!(is_date_taken_target(&["companyId", "userId", "date"]));
        assert!(is_date_taken_target(&["date", "companyId", "userId"]));
        assert!(is_date_taken_target(&["companyId,userId,date"]));
        assert!(!is_date_taken_target(&["companyId", "code"]));
        assert!(!is_date_taken_target(&[]));
    }

    #[test]
    fn date_taken_violation_from_pg() {
        assert!(is_date_taken_violation(Some(DATE_TAKEN_INDEX), None));
        assert!(is_date_taken_violation(
            Some("lain"),
            Some(r#"Key ("companyId", "userId", date)=(1, 2, 2026-10-22) already exists."#)
        ));
        assert!(!is_date_taken_violation(
            Some("overtimeRequest_uuid_key"),
            Some("Key (uuid)=(x) already exists.")
        ));
        assert!(!is_date_taken_violation(None, None));
    }

    #[tokio::test]
    async fn non_unique_error_passes_through() {
        let (status, _) = body_of(map_overtime_unique(sqlx::Error::RowNotFound)).await;
        assert_eq!(status, 500);
    }
}
