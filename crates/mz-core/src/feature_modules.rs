//! Port `src/feature-modules/feature-modules.service.ts` + `FeatureModuleInterceptor`.

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;

use crate::cache::{EpochCache, Keep, shared_err};
use crate::enums::FeatureModule;
use crate::{ApiError, ApiResult, AppState};

pub const MODULE_CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct CompanyModules {
    pub company_id: i32,
    pub modules: Vec<FeatureModule>,
}

/// `None` = user tanpa company (tidak di-cache, sama seperti Node).
pub type ModuleCache = EpochCache<i32, Option<Arc<CompanyModules>>>;

pub fn new_module_cache() -> ModuleCache {
    EpochCache::new(100_000)
}

/// `flagsToModules` — urutan selalu sama.
pub fn flags_to_modules(time_tracking: bool, sales_tracker: bool) -> Vec<FeatureModule> {
    let mut m = Vec::with_capacity(2);
    if time_tracking {
        m.push(FeatureModule::TIME_TRACKING);
    }
    if sales_tracker {
        m.push(FeatureModule::SALES_TRACKER);
    }
    m
}

async fn fetch(db: &sqlx::PgPool, user_id: i32) -> sqlx::Result<Option<Arc<CompanyModules>>> {
    let row = sqlx::query_as::<_, (i32, bool, bool)>(
        r#"SELECT uc."companyId", c."moduleTimeTrackingEnabled", c."moduleSalesTrackerEnabled"
           FROM "userCompany" uc JOIN company c ON c.id = uc."companyId"
           WHERE uc."userId" = $1 AND uc.status = 1
           ORDER BY uc."isDefault" DESC, uc.id ASC
           LIMIT 1"#,
    )
    .bind(user_id)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|(company_id, tt, st)| {
        Arc::new(CompanyModules {
            company_id,
            modules: flags_to_modules(tt, st),
        })
    }))
}

/// Modul aktif di company user. Kosong = user tanpa company.
pub async fn enabled_modules(state: &AppState, user_id: i32) -> ApiResult<Vec<FeatureModule>> {
    let db = state.db.clone();
    let entry = state
        .caches
        .modules
        .get_or_load(
            user_id,
            |v| v.as_ref().map(|_| Keep::ttl(MODULE_CACHE_TTL)),
            move || async move { fetch(&db, user_id).await.map_err(shared_err) },
        )
        .await
        .map_err(|e| ApiError::internal(anyhow::anyhow!("{e}")))?;
    Ok(entry.map(|e| e.modules.clone()).unwrap_or_default())
}

pub async fn has_any_module(
    state: &AppState,
    user_id: i32,
    modules: &[FeatureModule],
) -> ApiResult<bool> {
    let enabled = enabled_modules(state, user_id).await?;
    Ok(modules.iter().any(|m| enabled.contains(m)))
}

/// `invalidateCompany` — sesudah setelan modul company berubah.
/// Lokal + publish `company-modules`.
pub fn invalidate_company(state: &AppState, company_id: i32) {
    invalidate_company_local(&state.caches, company_id);
    crate::invalidation::publish(
        state,
        crate::invalidation::Message::CompanyModules { company_id },
    );
}

/// Hanya lokal (dipakai penerima pesan invalidasi).
pub fn invalidate_company_local(caches: &crate::state::Caches, company_id: i32) {
    caches
        .modules
        .invalidate_where(|_, v| v.as_ref().is_some_and(|m| m.company_id == company_id));
}

/// Keputusan interceptor. Daftar kosong (tanpa company) = lewat; service-nya
/// yang menjawab "no company found".
pub fn decide(required: &[FeatureModule], enabled: &[FeatureModule]) -> ApiResult<()> {
    if required.is_empty() || enabled.is_empty() {
        return Ok(());
    }
    if required.iter().any(|m| enabled.contains(m)) {
        return Ok(());
    }
    let labels: Vec<&str> = required.iter().map(FeatureModule::label).collect();
    Err(ApiError::coded(
        StatusCode::FORBIDDEN,
        "MODULE_DISABLED",
        format!("{} is not enabled for this company", labels.join(" / ")),
    ))
}
