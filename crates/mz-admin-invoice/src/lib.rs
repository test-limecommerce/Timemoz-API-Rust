//! Port `src/admin-invoice/**` (Mudah-API): `admin-invoices` untuk dashboard operator.
//!
//! Di-port: `GET /admin-invoices`, `GET /admin-invoices/{uuid}`, `POST …/{uuid}/void`,
//! `POST …/{uuid}/resend-email`. `GET …/{uuid}/pdf` dan `POST …/{uuid}/regenerate-pdf` ada di crate
//! `mz-billing` (generator PDF invoice bersama). Lihat `plan/deviations/mz-admin-invoice.md`.

use axum::Router;
use axum::routing::{get, post};
use mz_core::AppState;
use mz_core::gate::{RouteSpec, guard};

pub mod service;

/// `@PermissionExempt('separate-auth')` + `@AllowWhenSubscriptionLocked()`.
const OPERATOR: RouteSpec = RouteSpec::public().allow_locked();

/// Router seluruh route crate ini (path penuh, tanpa prefix).
pub fn router(state: &AppState) -> Router<AppState> {
    let g = |r| guard(state, OPERATOR, r);
    Router::new()
        .route("/admin-invoices", g(get(service::h_list)))
        .route("/admin-invoices/{uuid}", g(get(service::h_detail)))
        .route("/admin-invoices/{uuid}/void", g(post(service::h_void)))
        .route(
            "/admin-invoices/{uuid}/resend-email",
            g(post(service::h_resend_email)),
        )
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn router_tanpa_konflik() {
        if let Ok(state) = mz_core::AppState::for_test() {
            let _ = super::router(&state);
        }
    }
}
