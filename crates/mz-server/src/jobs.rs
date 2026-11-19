//! Registry cron Rust (Fase 4a). Semua job mati bawaan; aktifkan lewat env `MZ_JOBS`
//! (`all` atau daftar nama dipisah koma) SETELAH cron yang sama dimatikan di Node.

use mz_jobs::CronJob;

/// Semua cron yang dikenal `mudah-rs`.
pub fn registry() -> Vec<CronJob> {
    let mut jobs = Vec::new();
    jobs.extend(mz_screenshots::cron_jobs());
    jobs.extend(mz_timesheets::cron_jobs());
    jobs.extend(mz_projects::cron_jobs());
    jobs.extend(mz_auth::cron_jobs());
    jobs.extend(mz_audit::cron_jobs());
    jobs.extend(mz_overtime::cron_jobs());
    jobs.extend(mz_int_clickup::cron_jobs());
    jobs.extend(mz_int_jira::cron_jobs());
    jobs.extend(mz_int_gcal::cron_jobs());
    jobs.extend(mz_admin_mcp::cron_jobs());
    jobs.extend(mz_tasks_sync::cron_jobs());
    jobs.extend(mz_billing_tasks::cron_jobs());
    jobs.extend(mz_leave::cron_jobs());
    jobs.extend(mz_visit_forms::cron_jobs());
    jobs.extend(mz_store_visits::cron_jobs());
    jobs.extend(mz_misc::cron_jobs());
    jobs
}

/// Nama `MZ_JOBS` yang bukan cron, melainkan gerbang kerja latar di jalur request.
pub const GATES: &[&str] = &[
    // mz-auth: kirim GA4 `app_installed` langsung sesudah login (tanpa menunggu sweep).
    "analytics-ga",
    // mz-overtime: recalc lembur langsung sesudah timeline/approval berubah (nudge).
    "overtime-recalc",
];

/// Nama semua job (untuk pesan galat CLI).
pub fn names() -> Vec<&'static str> {
    registry().iter().map(|j| j.name).collect()
}
