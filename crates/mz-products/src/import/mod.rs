//! Import produk lewat Excel (§9 docs/product_master.md): 4 route
//! (`POST /v1/admin/products/import`, `GET .../imports`, `GET .../imports/{id}`,
//! `GET .../import/template`) + worker antrean `product-import` (`mz_core::queue`,
//! pengganti Bull `product-import`). Port `product-import*.ts`, `product-imports.service.ts`,
//! `safe-image-fetch.ts`, `import-file-exception.filter.ts`.

pub mod columns;
pub mod parser;
pub mod row;
pub mod safe_fetch;
pub mod service;
pub mod worker;
pub mod writer;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use mz_core::AppState;
use mz_core::gate::guard;

pub use worker::{handle, process_job, process_job_with, spawn_worker};

/// Route import (spec kelas `AdminProductsController` = [`crate::PRODUCTS`]).
pub fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/v1/admin/products/import",
            guard(
                state,
                crate::PRODUCTS,
                post(service::h_import).layer(DefaultBodyLimit::max(service::IMPORT_BODY_LIMIT)),
            ),
        )
        .route(
            "/v1/admin/products/imports",
            guard(state, crate::PRODUCTS, get(service::h_list)),
        )
        .route(
            "/v1/admin/products/imports/{id}",
            guard(state, crate::PRODUCTS, get(service::h_detail)),
        )
        .route(
            "/v1/admin/products/import/template",
            guard(state, crate::PRODUCTS, get(service::h_template)),
        )
}
