//! Port tahap S3 sales tracker: `src/store-visits/**` (Mudah-API) — toko, region (store groups),
//! jadwal kunjungan, kunjungan (admin & mobile), foto kunjungan, offline sync, cron sapu per jam.
//!
//! Modul bersama (`pub`, dipakai juga `mz-orders`; ringkasan di `plan/sales-mz-store-visits-api.md`):
//! - [`scope`] — `store-scope.service.ts` (tri-state wilayah, klausa SQL + predikat + audit `outOfScope`);
//! - [`export`] — `export-limits.const.ts`, `enums/export-format.enum.ts`, `export-quota.util.ts`,
//!   limiter export per company, pembantu sel/CSV/XLSX;
//! - [`enums`], [`geo_util`], [`plan_dates`] (`store-visit-plans.util.ts`), [`photo_url`], [`photos`].
//!
//! Deviasi: `plan/deviations/mz-store-visits.md`.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]

pub mod admin_visits;
pub mod enums;
pub mod export;
pub mod geo_util;
pub mod groups;
pub mod offline_sync;
pub mod photo_url;
pub mod photos;
pub mod plan_dates;
pub mod plans;
pub mod scope;
pub mod stores;
pub mod visits;

use axum::Router;
use mz_core::AppState;
use mz_core::enums::Role;

/// `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER)` — seluruh controller admin.
pub(crate) const ADMIN_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
];

/// `@Roles(SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER, STAFF)` — seluruh controller mobile.
pub(crate) const MOBILE_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
    Role::STAFF,
];

/// 51 route (path penuh, tanpa prefix). Juga mendaftarkan resolver reveal tabel `stores`.
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .merge(stores::routes(state))
        .merge(groups::routes(state))
        .merge(plans::routes(state))
        .merge(visits::routes(state))
        .merge(admin_visits::routes(state))
        .merge(offline_sync::routes(state))
}

/// Cron `StoreVisitsCron` (`10 * * * *`, mati bawaan sampai `MZ_JOBS`).
pub fn cron_jobs() -> Vec<mz_jobs::CronJob> {
    visits::cron_jobs()
}
