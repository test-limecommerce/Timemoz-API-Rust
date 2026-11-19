//! Controller: `roles.controller.ts`, `permissions-admin.controller.ts`,
//! `assignable-roles.controller.ts`, `permissions/me-permissions.controller.ts`.

use axum::Json;
use axum::extract::{FromRequestParts, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::IntoResponse;
use mz_audit::AuditHandle;
use mz_core::extract::{PathParams, ValidatedJson, ValidatedQuery};
use mz_core::uuid_rules::uuid_pipe;
use mz_core::{ApiError, AppState, RequestCtx};

use crate::dto::{
    CloneRoleDto, CreateRoleDto, DeleteRoleQueryDto, PreviewRoleDto, ReplaceRolePermissionsDto,
    UpdateRoleDto,
};
use crate::entity::Data;
use crate::error::RolesResult;
use crate::preview::build_me_permissions;
use crate::service;
use mz_core::feature_registry::{FEATURES, ROLE_TEMPLATES};

/// `@Param('uuid', ParseUUIDPipe)`. Extractor bagian (bukan body), jadi galatnya mendahului
/// ValidationPipe body/query — sama dengan Nest (pipe uuid menolak lebih dulu).
pub struct RoleUuid(pub String);

impl<S: Send + Sync> FromRequestParts<S> for RoleUuid {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let PathParams(raw) = PathParams::<String>::from_request_parts(parts, state).await?;
        uuid_pipe(raw).map(Self)
    }
}

/// `GET|PATCH|DELETE /v1/admin/roles/preview`: path statis `preview` hanya punya `POST`; di Nest
/// method lain jatuh ke `:uuid` dan `ParseUUIDPipe` menolak `preview`.
pub async fn preview_as_uuid() -> ApiError {
    ApiError::bad_request("Validation failed (uuid is expected)")
}

/// Node: src/roles/roles.controller.ts:69
pub async fn list(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> RolesResult<impl IntoResponse> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(Data {
        data: service::list(&state, user_id).await?,
    }))
}

/// Node: src/roles/roles.controller.ts:78
pub async fn create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(dto): ValidatedJson<CreateRoleDto>,
) -> RolesResult<impl IntoResponse> {
    let user_id = ctx.require_user_id()?;
    let data = service::create(&state, &audit, user_id, &dto).await?;
    Ok((StatusCode::CREATED, Json(Data { data })))
}

/// Node: src/roles/roles.controller.ts:89
pub async fn preview(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedJson(dto): ValidatedJson<PreviewRoleDto>,
) -> RolesResult<impl IntoResponse> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(Data {
        data: service::preview(&state, user_id, &dto).await?,
    }))
}

/// Node: src/roles/roles.controller.ts:98
pub async fn find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RoleUuid(uuid): RoleUuid,
) -> RolesResult<impl IntoResponse> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(Data {
        data: service::detail(&state, user_id, &uuid).await?,
    }))
}

/// Node: src/roles/roles.controller.ts:112
pub async fn update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    RoleUuid(uuid): RoleUuid,
    ValidatedJson(dto): ValidatedJson<UpdateRoleDto>,
) -> RolesResult<impl IntoResponse> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(Data {
        data: service::update(&state, &audit, user_id, &uuid, &dto).await?,
    }))
}

/// Node: src/roles/roles.controller.ts:127
pub async fn replace_permissions(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    RoleUuid(uuid): RoleUuid,
    ValidatedJson(dto): ValidatedJson<ReplaceRolePermissionsDto>,
) -> RolesResult<impl IntoResponse> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(Data {
        data: service::replace_permissions(&state, &audit, user_id, &uuid, &dto).await?,
    }))
}

/// Node: src/roles/roles.controller.ts:152
pub async fn customize(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    RoleUuid(uuid): RoleUuid,
    ValidatedJson(dto): ValidatedJson<ReplaceRolePermissionsDto>,
) -> RolesResult<impl IntoResponse> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(Data {
        data: service::customize_built_in(&state, &audit, user_id, &uuid, &dto).await?,
    }))
}

/// Node: src/roles/roles.controller.ts:174
pub async fn revert(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    RoleUuid(uuid): RoleUuid,
) -> RolesResult<impl IntoResponse> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(Data {
        data: service::revert_built_in_override(&state, &audit, user_id, &uuid).await?,
    }))
}

/// Node: src/roles/roles.controller.ts:190
pub async fn clone(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    RoleUuid(uuid): RoleUuid,
    ValidatedJson(dto): ValidatedJson<CloneRoleDto>,
) -> RolesResult<impl IntoResponse> {
    let user_id = ctx.require_user_id()?;
    let data = service::clone(&state, &audit, user_id, &uuid, &dto).await?;
    Ok((StatusCode::CREATED, Json(Data { data })))
}

/// Node: src/roles/roles.controller.ts:209
pub async fn remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    RoleUuid(uuid): RoleUuid,
    ValidatedQuery(query): ValidatedQuery<DeleteRoleQueryDto>,
) -> RolesResult<impl IntoResponse> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(Data {
        data: service::remove(&state, &audit, user_id, &uuid, &query).await?,
    }))
}

/// Node: src/roles/roles.controller.ts:224
pub async fn impact(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RoleUuid(uuid): RoleUuid,
) -> RolesResult<impl IntoResponse> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(Data {
        data: service::impact(&state, user_id, &uuid).await?,
    }))
}

/// Node: src/roles/permissions-admin.controller.ts:34
pub async fn features() -> impl IntoResponse {
    Json(Data { data: FEATURES })
}

/// Node: src/roles/permissions-admin.controller.ts:41
pub async fn templates() -> impl IntoResponse {
    Json(Data {
        data: &*ROLE_TEMPLATES,
    })
}

/// Node: src/roles/assignable-roles.controller.ts:34
pub async fn assignable(
    State(state): State<AppState>,
    ctx: RequestCtx,
) -> RolesResult<impl IntoResponse> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(Data {
        data: service::list_assignable(&state, user_id).await?,
    }))
}

/// Node: src/permissions/me-permissions.controller.ts:44 — membaca `req.access` yang sudah
/// dihitung gerbang permission (exempt `self-service`), tanpa query tambahan.
pub async fn me_permissions(ctx: RequestCtx) -> impl IntoResponse {
    Json(Data {
        data: build_me_permissions(ctx.access().map(|a| a.as_ref())),
    })
}
