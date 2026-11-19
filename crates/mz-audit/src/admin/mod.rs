//! Sisi BACA/RESTORE/RETENSI audit log (Fase 4a): port `admin-audit-logs.controller.ts`,
//! `audit-logs-query.service.ts`, `audit-restore.service.ts`, `audit-field-labels.ts`,
//! `audit-prune.service.ts`. Penulisnya (middleware Prisma) ada di modul-modul induk.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use mz_core::enums::Role;
use mz_core::gate::guard;
use mz_core::plan::PlanFeature;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx, RouteSpec};

use self::dto::{BinQuery, ListQuery, RawQuery, parse_entity_query};
use crate::handle::AuditHandle;
use crate::scope::EntityType;

mod dto;
pub mod labels;
pub mod prune;
mod query;
pub mod restore;

pub use prune::{AUDIT_PRUNE_BATCH_SIZE, PruneResult, cron_jobs, prune_expired};
pub use restore::AUDIT_RETENTION_DAYS;

/// `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER)` pada kelas controller.
const READERS: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
];

/// `@Roles` di `POST :uuid/restore`: SENGAJA tanpa `MANAGER` (§0.11, §7.4) — jangan "dirapikan".
const RESTORERS: &[Role] = &[Role::SYSTEM_ADMIN, Role::SUPER_ADMIN, Role::ADMIN];

/// `@RequireFeature('audit-logs')` + `@UseGuards(JwtAuthGuard, RolesGuard)` (kelas).
/// `@RequirePlanFeature(PlanFeature.AUDIT_LOG)` (kelas) → `RouteSpec::plan_features`.
const READ: RouteSpec = RouteSpec::jwt()
    .roles(READERS)
    .feature("audit-logs")
    .plan_features(&[PlanFeature::AuditLog]);
const RESTORE: RouteSpec = RouteSpec::jwt()
    .roles(RESTORERS)
    .feature("audit-logs")
    .plan_features(&[PlanFeature::AuditLog]);

/// `GET /v1/admin/audit-logs` — Node: src/audit-logs/admin-audit-logs.controller.ts:118
async fn h_find_all(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(pairs): RawQuery,
) -> ApiResult<Json<serde_json::Value>> {
    let user_id = ctx.require_user_id()?;
    let query = ListQuery::parse(&pairs)?;
    Ok(Json(query::find_all(&state.db, user_id, &query).await?))
}

/// `GET /v1/admin/audit-logs/recycle-bin` — Node: admin-audit-logs.controller.ts:140
async fn h_recycle_bin(
    State(state): State<AppState>,
    ctx: RequestCtx,
    RawQuery(pairs): RawQuery,
) -> ApiResult<Json<serde_json::Value>> {
    let user_id = ctx.require_user_id()?;
    let query = BinQuery::parse(&pairs)?;
    Ok(Json(
        query::find_recycle_bin(&state.db, user_id, &query).await?,
    ))
}

/// `POST /v1/admin/audit-logs/:uuid/restore` (200) — Node: admin-audit-logs.controller.ts:196
async fn h_restore(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    Path(uuid): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(
        restore::restore(&state, &audit, user_id, &uuid).await?,
    ))
}

/// `GET /v1/admin/audit-logs/:uuid` — Node: admin-audit-logs.controller.ts:213
async fn h_find_one(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(uuid): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let user_id = ctx.require_user_id()?;
    Ok(Json(query::find_one(&state.db, user_id, &uuid).await?))
}

/// `GET /v1/admin/audit-logs/entities/:entityType/:entityUuid` — Node: admin-audit-logs.controller.ts:233
async fn h_by_entity(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path((entity_type, entity_uuid)): Path<(String, String)>,
    RawQuery(pairs): RawQuery,
) -> ApiResult<Json<serde_json::Value>> {
    let user_id = ctx.require_user_id()?;
    // `ParseEnumPipe(AuditEntityType)` jalan sebelum validasi query.
    if EntityType::from_name(&entity_type).is_none() {
        return Err(ApiError::bad_request(
            "Validation failed (enum string is expected)",
        ));
    }
    let paging = parse_entity_query(&pairs)?;
    Ok(Json(
        query::find_by_entity(&state.db, user_id, &entity_type, &entity_uuid, paging).await?,
    ))
}

/// Seluruh route `v1/admin/audit-logs` (5 route).
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/v1/admin/audit-logs", guard(state, READ, get(h_find_all)))
        .route(
            "/v1/admin/audit-logs/recycle-bin",
            guard(state, READ, get(h_recycle_bin)),
        )
        .route(
            "/v1/admin/audit-logs/entities/{entity_type}/{entity_uuid}",
            guard(state, READ, get(h_by_entity)),
        )
        .route(
            "/v1/admin/audit-logs/{uuid}/restore",
            guard(state, RESTORE, post(h_restore)),
        )
        .route(
            "/v1/admin/audit-logs/{uuid}",
            guard(state, READ, get(h_find_one)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    async fn status(app: &Router, method: &str, uri: &str) -> u16 {
        let req = http::Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(req).await.unwrap().status().as_u16()
    }

    /// Kelima route terdaftar tanpa konflik path, dan semuanya di belakang JWT (401 tanpa token).
    #[tokio::test]
    async fn five_routes_are_registered_behind_jwt() {
        let state = AppState::for_test().unwrap();
        let app = router(&state).with_state(state.clone());
        for (m, uri) in [
            ("GET", "/v1/admin/audit-logs"),
            ("GET", "/v1/admin/audit-logs/recycle-bin"),
            (
                "GET",
                "/v1/admin/audit-logs/3c1a2b4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d",
            ),
            (
                "GET",
                "/v1/admin/audit-logs/entities/store/9f1c2d3e-4a5b-6c7d-8e9f-0a1b2c3d4e5f",
            ),
            (
                "POST",
                "/v1/admin/audit-logs/3c1a2b4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d/restore",
            ),
        ] {
            assert_eq!(status(&app, m, uri).await, 401, "{m} {uri}");
        }
        assert_eq!(status(&app, "GET", "/v1/admin/other").await, 404);
    }
}
