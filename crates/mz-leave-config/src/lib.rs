//! Port `src/leave-types/**` (7 route) dan `src/leave-policies/**` (20 route) dari Mudah-API.
//!
//! Kedua controller: `@RequireFeature('leave-config')`, `@SkipThrottle()`,
//! `@UseGuards(JwtAuthGuard, RolesGuard)`; `@Roles` per handler (baca jenis cuti: semua peran
//! company; selainnya SYSTEM_ADMIN..MANAGER). Aksi izin diturunkan dari method HTTP.
//!
//! Domain cuti (saldo, kebijakan, antrean `leave-schedule`) dipakai dari `mz-leave`; crate ini
//! hanya route + validasi DTO + SQL konfigurasi. Model cuti TIDAK ber-audit (`mz-audit` tidak
//! meliputi `leave*`) dan Node tidak memakai cache untuk modul ini.
//! Deviasi: `plan/deviations/mz-leave-config.md`.

pub mod balances;
mod common;
pub mod dto;
pub mod employees;
pub mod policy;
pub mod types;
pub mod validate;

use axum::Router;
use axum::routing::{delete, get, patch, post, put};
use mz_core::enums::Role;
use mz_core::gate::guard;
use mz_core::{AppState, RequestCtx, RouteSpec};

/// `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER, STAFF)`.
const ALL_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
    Role::STAFF,
];
/// `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER)`.
const MANAGE_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
];

/// Kelas controller: `@RequireFeature('leave-config')`.
const BASE: RouteSpec = RouteSpec::jwt().feature("leave-config");
const READ_ALL: RouteSpec = BASE.roles(ALL_ROLES);
const MANAGE: RouteSpec = BASE.roles(MANAGE_ROLES);

/// Method tulis/baca pada path statis `seed` yang di Node ditelan `:id` (`GET|PUT|DELETE
/// /leave-types/seed` → `id = 'seed'` → `parseInt` NaN → 500 sesudah company di-resolve).
async fn seed_as_id_get(
    axum::extract::State(state): axum::extract::State<AppState>,
    ctx: RequestCtx,
) -> mz_core::ApiResult<axum::Json<types::LeaveTypeRow>> {
    types::find_one_inner(&state, &ctx, "seed").await
}

async fn seed_as_id_put(
    axum::extract::State(state): axum::extract::State<AppState>,
    ctx: RequestCtx,
    mz_core::extract::ValidatedJson(dto): mz_core::extract::ValidatedJson<dto::UpdateLeaveTypeDto>,
) -> mz_core::ApiResult<axum::Json<types::LeaveTypeRow>> {
    types::update_inner(&state, &ctx, "seed", dto).await
}

async fn seed_as_id_delete(
    axum::extract::State(state): axum::extract::State<AppState>,
    ctx: RequestCtx,
) -> mz_core::ApiResult<axum::Json<common::Success>> {
    types::remove_inner(&state, &ctx, "seed").await
}

pub fn router(state: &AppState) -> Router<AppState> {
    let g = |spec, route| guard(state, spec, route);
    Router::new()
        // ── leave-types (7) ──
        .route("/leave-types", g(READ_ALL, get(types::list)))
        .route("/leave-types", g(MANAGE, post(types::create)))
        .route("/leave-types/seed", g(MANAGE, post(types::seed)))
        .route("/leave-types/seed", g(READ_ALL, get(seed_as_id_get)))
        .route("/leave-types/seed", g(MANAGE, put(seed_as_id_put)))
        .route("/leave-types/seed", g(MANAGE, delete(seed_as_id_delete)))
        .route("/leave-types/{id}", g(READ_ALL, get(types::find_one)))
        .route("/leave-types/{id}", g(MANAGE, put(types::update)))
        .route("/leave-types/{id}", g(MANAGE, delete(types::remove)))
        .route("/leave-types/{id}/toggle", g(MANAGE, patch(types::toggle)))
        // ── leave-policies: kebijakan (3) ──
        .route("/leave-policies", g(MANAGE, get(policy::get_policy)))
        .route("/leave-policies", g(MANAGE, put(policy::upsert_policy)))
        .route(
            "/leave-policies/admins",
            g(MANAGE, get(policy::list_admins)),
        )
        // ── override karyawan (4) ──
        .route(
            "/leave-policies/employees",
            g(MANAGE, get(employees::list_employee_policies)),
        )
        .route(
            "/leave-policies/employees/{user_id}",
            g(MANAGE, get(employees::get_employee_policy)),
        )
        .route(
            "/leave-policies/employees/{user_id}",
            g(MANAGE, put(employees::upsert_employee_policy)),
        )
        .route(
            "/leave-policies/employees/{user_id}",
            g(MANAGE, delete(employees::delete_employee_policy)),
        )
        // ── tier (7) ──
        .route(
            "/leave-policies/tiers",
            g(MANAGE, get(employees::list_tiers)),
        )
        .route(
            "/leave-policies/tiers",
            g(MANAGE, post(employees::create_tier)),
        )
        .route(
            "/leave-policies/tiers/employees/{user_id}",
            g(MANAGE, get(employees::get_employee_tier)),
        )
        .route(
            "/leave-policies/tiers/employees/{user_id}",
            g(MANAGE, delete(employees::remove_employee_tier)),
        )
        .route(
            "/leave-policies/tiers/{tier_id}",
            g(MANAGE, get(employees::get_tier)),
        )
        .route(
            "/leave-policies/tiers/{tier_id}",
            g(MANAGE, put(employees::update_tier)),
        )
        .route(
            "/leave-policies/tiers/{tier_id}",
            g(MANAGE, delete(employees::delete_tier)),
        )
        .route(
            "/leave-policies/tiers/{tier_id}/assign/{user_id}",
            g(MANAGE, put(employees::assign_employee_tier)),
        )
        // ── saldo (5) ──
        .route(
            "/leave-policies/balances",
            g(MANAGE, get(balances::get_balances)),
        )
        .route(
            "/leave-policies/balances/initialize",
            g(MANAGE, post(balances::bulk_initialize)),
        )
        .route(
            "/leave-policies/balances/initialize/{user_id}",
            g(MANAGE, post(balances::initialize_balance)),
        )
        .route(
            "/leave-policies/balances/{user_id}/adjust",
            g(MANAGE, patch(balances::adjust_balance)),
        )
        .route(
            "/leave-policies/balances/{user_id}/adjustments",
            g(MANAGE, get(balances::get_adjustments)),
        )
}
