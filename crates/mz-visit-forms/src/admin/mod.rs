//! Port `admin-store-visit-forms.controller.ts` (26 route) + `admin-store-visit-forms.service.ts`.
//!
//! - [`templates`]: template, assignee, versi, validate, pratinjau PDF, aset kop/kaki, katalog.
//! - [`submissions`]: daftar/detail/koreksi/revisi submission, PDF, proxy file.

pub mod submissions;
pub mod templates;

use axum::Router;
use mz_core::enums::{FeatureModule, Role};
use mz_core::plan::PlanFeature;
use mz_core::rate_limit::RateRule;
use mz_core::{ApiResult, AppState, RouteSpec};

pub const ADMIN_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
];

/// Gerbang kelas `AdminStoreVisitFormsController`: `@RequireFeature('store-visit-forms')`,
/// `@RequireModule(SALES_TRACKER)`, `@RequirePlanFeature(FORM)`, `@Roles(SA, SUPER, ADMIN,
/// MANAGER)`, `@SkipThrottle()` (tanpa efek). Tambah `.rate_limits(..)` per route sesuai
/// `main.ts`, `.action(..)` untuk `@RequirePermission` eksplisit.
pub const ADMIN: RouteSpec = RouteSpec::jwt()
    .feature("store-visit-forms")
    .modules(&[FeatureModule::SALES_TRACKER])
    .plan_features(&[PlanFeature::Form])
    .roles(ADMIN_ROLES);

pub const MINUTE: u64 = 60_000;
pub const HOUR: u64 = 60 * MINUTE;

/// Limiter `main.ts` (per token, `passOnStoreError: false`).
pub mod limits {
    use super::*;
    pub const READ: RateRule = RateRule::token("rl:admin-store-visit-forms-read", 60, MINUTE);
    pub const WRITE: RateRule = RateRule::token("rl:admin-store-visit-forms-write", 60, MINUTE);
    pub const VALIDATE: RateRule =
        RateRule::token("rl:admin-store-visit-forms-validate", 120, MINUTE);
    pub const PDF_PREVIEW: RateRule =
        RateRule::token("rl:admin-store-visit-forms-pdf-preview", 20, MINUTE);
    pub const ASSET_UPLOAD: RateRule =
        RateRule::token("rl:admin-store-visit-forms-asset-upload", 30, HOUR);
    pub const ASSET_CONFIRM: RateRule =
        RateRule::token("rl:admin-store-visit-forms-asset-confirm", 30, HOUR);
    pub const CORRECT: RateRule = RateRule::token("rl:admin-store-visit-forms-correct", 20, MINUTE);
}

/// `getCompanyId(userId)` = `requireCompanyId` (403 bila tanpa company).
pub async fn company_id(state: &AppState, user_id: i32) -> ApiResult<i32> {
    mz_core::company::require_company_id(&state.db, user_id).await
}

/// `getTimeZone(companyId)` — `company?.timeZone || 'UTC'`.
pub async fn time_zone(state: &AppState, company_id: i32) -> ApiResult<String> {
    let tz: Option<Option<String>> =
        sqlx::query_scalar(r#"SELECT "timeZone" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(&state.db)
            .await?;
    Ok(tz
        .flatten()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "UTC".to_owned()))
}

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .merge(templates::router(state))
        .merge(submissions::router(state))
}
