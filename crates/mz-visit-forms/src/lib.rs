//! Port modul form kunjungan Mudah-API `src/store-visit-forms/**`:
//! - `admin-store-visit-forms.controller.ts` (26 route `/v1/admin/store-visit-forms/**`),
//! - `mobile-store-visit-forms.controller.ts` (7 route `/v1/mobile/store-visits/:visitUuid/forms/**`),
//! - `store-visit-form-revisers.controller.ts` (2 route `/v1/admin/store-visit-forms/revisers`),
//! - `mobile-store-visit-form-file.controller.ts` (1 route tanpa JWT, URL bertanda tangan),
//! - cron `store-visit-forms.cron.ts` ([`cron_jobs`]).
//!
//! Modul store-visits belum di-port: tabel `storeVisit` dibaca lewat SQL di crate ini
//! (titik temu tahap S3: `plan/sales-mz-visit-forms-api.md`). Deviasi:
//! `plan/deviations/mz-visit-forms.md`.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]

pub mod admin;
pub mod answers_validator;
pub mod consts;
pub mod cron;
pub mod dynamic_options;
pub mod errors;
pub mod file_url;
pub mod files;
pub mod layout_validator;
pub mod leaf_validator;
pub mod mobile;
pub mod node_path;
pub mod pdf;
pub mod plan_limit;
pub mod preview_sample;
pub mod revise_mode;
pub mod revise_policy;
pub mod revisers;
pub mod schema_validator;
pub mod types;
pub mod value_util;
pub mod visibility;

use axum::Router;
use mz_core::AppState;

pub use cron::cron_jobs;

/// Semua route modul ini (36).
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .merge(revisers::router(state))
        .merge(admin::router(state))
        .merge(mobile::router(state))
        .merge(files::router(state))
}
