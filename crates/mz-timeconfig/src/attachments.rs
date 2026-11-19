//! Lampiran manual time (`streamAttachment`, `attachmentUrl`, `resolveAttachment`,
//! `canViewAttachment`) — dua route berkas `ManualTimeApprovalsController`.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use mz_core::enums::sees_whole_company;
use mz_core::extract::PathParams;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use mz_files::{
    SIGNED_URL_RESPONSE_CACHE_CONTROL, SignedObjectOpts, StreamObjectOptions,
    get_signed_object_url, load_file_requester, object_key_of, stream_object,
};

use crate::approvals::is_active_approver;
use crate::util::parse_uuid;

#[derive(sqlx::FromRow)]
struct AttachmentRow {
    file_name: String,
    file_fullpath: String,
    mime_type: String,
    timeline_uuid: String,
    project_id: i32,
    owner_id: i32,
    owner_company_id: Option<i32>,
}

/// Hasil `resolveAttachment`.
pub struct Resolved {
    pub key: String,
    pub file_name: String,
    pub mime_type: String,
}

fn not_found() -> ApiError {
    ApiError::not_found("Attachment not found")
}

/// `canViewAttachment`: pemilik, approver aktif project, atau role `seesWholeCompany`
/// di company pemilik entry.
async fn can_view(state: &AppState, requester_id: i32, a: &AttachmentRow) -> ApiResult<bool> {
    if a.owner_id == requester_id {
        return Ok(true);
    }
    if is_active_approver(&state.db, requester_id, a.project_id).await? {
        return Ok(true);
    }
    let requester = load_file_requester(&state.db, requester_id).await?;
    Ok(sees_whole_company(requester.role_code.as_deref())
        && a.owner_company_id.is_some()
        && requester.company_id == a.owner_company_id)
}

/// Node: src/manual-time-approvals/manual-time-approvals.service.ts:640 (`resolveAttachment`). Melempar 404.
pub async fn resolve_attachment(
    state: &AppState,
    uuid: &str,
    requester_id: i32,
) -> ApiResult<Resolved> {
    let row: Option<AttachmentRow> = sqlx::query_as(
        r#"SELECT a."fileName" AS file_name, a."fileFullpath" AS file_fullpath, a."mimeType" AS mime_type,
                  t.uuid::text AS timeline_uuid, t."projectId" AS project_id, tr."userId" AS owner_id,
                  (SELECT uc."companyId" FROM "userCompany" uc
                    WHERE uc."userId" = tr."userId" AND uc.status = 1
                    ORDER BY uc."isDefault" DESC, uc.id ASC LIMIT 1) AS owner_company_id
           FROM "manualTimeAttachment" a
           JOIN "trackingTimeline" t ON t.id = a."trackingTimelineId"
           JOIN tracking tr ON tr.id = t."trackingId"
           WHERE a.uuid = $1::uuid AND a.status = 1 AND t.status IN (0, 1)
           LIMIT 1"#,
    )
    .bind(uuid)
    .fetch_optional(&state.db)
    .await?;
    let a = row.ok_or_else(not_found)?;
    if !can_view(state, requester_id, &a).await? {
        return Err(not_found());
    }
    // Key dari DB dan wajib di bawah `manual-time/<uuid timeline>/`.
    let key = object_key_of(state, &a.file_fullpath).map_err(|_| not_found())?;
    if !key.starts_with(&format!("manual-time/{}/", a.timeline_uuid)) {
        return Err(not_found());
    }
    Ok(Resolved {
        key,
        file_name: a.file_name,
        mime_type: a.mime_type,
    })
}

/// Node: src/manual-time-approvals/manual-time-approvals.controller.ts:119 (`attachmentFile`)
pub async fn h_file(
    State(state): State<AppState>,
    ctx: RequestCtx,
    headers: HeaderMap,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Response> {
    let uuid = parse_uuid(uuid)?;
    let r = resolve_attachment(&state, &uuid, ctx.require_user_id()?).await?;
    stream_object(
        &state,
        &r.key,
        Some(&headers),
        StreamObjectOptions {
            filename: Some(r.file_name),
            ..StreamObjectOptions::default()
        },
    )
    .await
}

/// Node: src/manual-time-approvals/manual-time-approvals.controller.ts:152 (`attachmentFileUrl`)
pub async fn h_file_url(
    State(state): State<AppState>,
    ctx: RequestCtx,
    PathParams(uuid): PathParams<String>,
) -> ApiResult<Response> {
    let uuid = parse_uuid(uuid)?;
    let r = resolve_attachment(&state, &uuid, ctx.require_user_id()?).await?;
    let url = get_signed_object_url(
        &state,
        &r.key,
        &SignedObjectOpts {
            content_type: Some(Some(r.mime_type.as_str())),
            filename: Some(r.file_name.as_str()),
        },
        chrono::Utc::now().timestamp_millis(),
    )
    .await?;
    let mut res = Json(url).into_response();
    res.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(SIGNED_URL_RESPONSE_CACHE_CONTROL),
    );
    Ok(res)
}
