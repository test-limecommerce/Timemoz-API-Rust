//! Selfie shift (Node: `mobile-clock.service.ts:802-893`, `selfie-punch-type.ts`).

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Punch {
    In,
    Out,
}

/// `SELFIE_PUNCH_TYPE_VALUES`.
pub const PUNCH_VALUES: [&str; 4] = ["in", "out", "clockIn", "clockOut"];

/// `parseSelfiePunchType(raw)` (route admin): `undefined`/`in`/`clockIn` → in,
/// `out`/`clockOut` → out, selain itu 400.
pub fn parse_admin_punch(raw: Option<&Value>) -> ApiResult<Punch> {
    match raw {
        None => Ok(Punch::In),
        Some(Value::String(s)) if s == "in" || s == "clockIn" => Ok(Punch::In),
        Some(Value::String(s)) if s == "out" || s == "clockOut" => Ok(Punch::Out),
        Some(_) => Err(ApiError::bad_request(format!(
            "type must be one of: {}",
            PUNCH_VALUES.join(", ")
        ))),
    }
}

/// Route mobile: `@Query('type') type = 'in'` lalu `punchType === 'in' ? in : out`.
pub fn parse_mobile_punch(raw: Option<&Value>) -> Punch {
    match raw {
        None => Punch::In,
        Some(Value::String(s)) if s == "in" => Punch::In,
        Some(_) => Punch::Out,
    }
}

fn pick(keys: (Option<String>, Option<String>), punch: Punch) -> ApiResult<String> {
    let key = match punch {
        Punch::In => keys.0,
        Punch::Out => keys.1,
    };
    key.filter(|k| !k.is_empty())
        .ok_or_else(|| ApiError::bad_request("No selfie available for this punch"))
}

/// `downloadSelfie(userId, shiftUuid, punch)` — hanya shift milik sendiri (tanpa batas company).
pub async fn own_key(
    state: &AppState,
    user_id: i32,
    shift_uuid: &str,
    punch: Punch,
) -> ApiResult<String> {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        r#"SELECT "clockInSelfieKey", "clockOutSelfieKey" FROM "mobileShift"
            WHERE uuid = $1::uuid AND "userId" = $2 LIMIT 1"#,
    )
    .bind(shift_uuid)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    let row = row.ok_or_else(|| ApiError::bad_request("Shift not found"))?;
    pick(row, punch)
}

/// `resolveSelfieKeyAdmin(adminUserId, shiftUuid, punch)` — shift di company admin.
pub async fn admin_key(
    state: &AppState,
    user_id: i32,
    shift_uuid: &str,
    punch: Punch,
) -> ApiResult<String> {
    let company_id = mz_core::company::require_company_id(&state.db, user_id).await?;
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        r#"SELECT "clockInSelfieKey", "clockOutSelfieKey" FROM "mobileShift"
            WHERE uuid = $1::uuid AND "companyId" = $2 LIMIT 1"#,
    )
    .bind(shift_uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let row = row.ok_or_else(|| ApiError::bad_request("Shift not found"))?;
    pick(row, punch)
}

/// `res.set('Content-Type','image/jpeg'); res.set('Cache-Control','no-store'); res.send(buffer)`.
pub async fn send_jpeg(state: &AppState, key: &str) -> ApiResult<Response> {
    let bytes = mz_files::download_buffer(state, key).await?;
    let len = bytes.len();
    let mut resp = (StatusCode::OK, bytes).into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    Ok(resp)
}

/// `selfieUrlAdmin`: signed URL GCS, selalu disajikan sebagai JPEG.
pub async fn signed_url(state: &AppState, key: &str) -> ApiResult<mz_files::SignedReadUrl> {
    let opts = mz_files::SignedObjectOpts {
        content_type: Some(Some("image/jpeg")),
        filename: None,
    };
    Ok(
        mz_files::get_signed_object_url(state, key, &opts, chrono::Utc::now().timestamp_millis())
            .await?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn admin_punch_values() {
        assert_eq!(parse_admin_punch(None).ok(), Some(Punch::In));
        assert_eq!(
            parse_admin_punch(Some(&json!("clockIn"))).ok(),
            Some(Punch::In)
        );
        assert_eq!(
            parse_admin_punch(Some(&json!("clockOut"))).ok(),
            Some(Punch::Out)
        );
        assert_eq!(
            parse_admin_punch(Some(&json!("out"))).ok(),
            Some(Punch::Out)
        );
        match parse_admin_punch(Some(&json!("x"))) {
            Err(ApiError::Message { message, .. }) => {
                assert_eq!(message, "type must be one of: in, out, clockIn, clockOut");
            }
            other => assert!(other.is_err(), "harus 400"),
        }
        assert!(parse_admin_punch(Some(&json!(["in"]))).is_err());
    }

    #[test]
    fn mobile_punch_defaults() {
        assert_eq!(parse_mobile_punch(None), Punch::In);
        assert_eq!(parse_mobile_punch(Some(&json!("in"))), Punch::In);
        assert_eq!(parse_mobile_punch(Some(&json!("clockIn"))), Punch::Out);
    }
}
