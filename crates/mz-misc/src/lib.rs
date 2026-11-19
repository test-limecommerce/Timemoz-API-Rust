//! Port `src/demo-requests/*` (publik, `POST /demo-requests`), `src/quickstart/*`
//! (`GET /v1/quickstart`, `POST /v1/quickstart/finish`), serta dua cron email onboarding:
//! `src/quickstart-reminder/*` (`quickstart-reminder`, :20) dan
//! `src/sales-onboarding-email/*` (`sales-onboarding-email`, :25), serta laporan monitoring
//! Discord `src/monitoring/*` ([`monitoring::spawn`], digerbang `MONITORING_CRON_ENABLED`,
//! bukan `MZ_JOBS`).
//!
//! Deviasi: `plan/deviations/mz-misc.md`.

pub mod demo_requests;
pub mod monitoring;
pub mod quickstart;
pub mod quickstart_reminder;
pub mod sales_onboarding;

use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use mz_core::AppState;
use mz_jobs::CronJob;

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .merge(demo_requests::router(state))
        .merge(quickstart::router(state))
}

/// Cron modul ini (mati bawaan sampai `MZ_JOBS`).
pub fn cron_jobs() -> Vec<CronJob> {
    vec![
        quickstart_reminder::cron_job(),
        sales_onboarding::cron_job(),
    ]
}

/// Pengirim email produksi (Mailtrap lewat `mz_core::mail`) untuk kedua cron.
#[derive(Clone, Copy)]
pub struct Mailtrap<'a>(pub &'a AppState);

/// Padanan flag `running` cron Node: dilepas saat di-drop, termasuk saat future dibatalkan
/// (timeout) — padanan `finally`.
pub(crate) struct RunGuard(&'static AtomicBool);

impl RunGuard {
    pub(crate) fn try_begin(flag: &'static AtomicBool) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| Self(flag))
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use serde_json::Value;

    /// Render template `templates/mail/<name>.hbs` (root workspace) dengan pengaturan yang
    /// sama dengan `mz_core::mail::render` (handlebars.js tidak strict).
    #[allow(clippy::panic)]
    pub fn render_template(name: &str, ctx: &Value) -> String {
        let path = format!(
            "{}/../../templates/mail/{name}.hbs",
            env!("CARGO_MANIFEST_DIR")
        );
        let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let mut h = handlebars::Handlebars::new();
        h.set_strict_mode(false);
        h.render_template(&src, ctx)
            .unwrap_or_else(|e| panic!("render {name}: {e}"))
    }
}
