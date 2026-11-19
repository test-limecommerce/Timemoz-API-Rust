//! Port cron billing `TasksService` (`src/tasks/tasks.service.ts`, Mudah-API) + 5 route operator
//! `POST /admin/*/force*` (`src/admin/admin.controller.ts`).
//!
//! | Node | Rust |
//! |---|---|
//! | `handlePaymentFailureEscalation` (:402) | [`escalation::run`] |
//! | `handleDownloadReminder` (:574) | [`reminders::download_reminder`] |
//! | `handleTrialEndingReminder` (:662) / `handleTrialEndedReminder` (:711) | [`reminders::trial_ending_reminder`] / [`reminders::trial_ended_reminder`] |
//! | `handleManualRenewalReminder` (:772) | [`reminders::manual_renewal_reminder`] |
//! | `handleInvoiceScheduler` (:864), `handleInvoiceSchedulerForce` (:1320), `handleAnnualProrationForce` (:1164) | [`scheduler`] |
//! | `handlePendingPlanChangeForce` (:918) | [`plan_change::run`] |
//! | `handleUnpaidProratedUsersExpiration[Force]` (:2027) | [`proration_expiry::run`] |
//! | `handleSubscriptionExpiration[Force]` (:2283) | [`expiration::run`] |
//!
//! Semua fungsi menerima [`Ctx`] yang membawa jam (`now`), klien PayPal, pengirim email, dan
//! handle audit, supaya bisa diuji dengan jam yang dikendalikan dan PayPal tiruan.
//! Deviasi: `plan/deviations/mz-billing-tasks.md`.

#![cfg_attr(
    test,
    allow(
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used
    )
)]

pub mod escalation;
pub mod expiration;
pub mod jsdate;
pub mod mail;
pub mod plan_change;
pub mod proration_expiry;
pub mod reminders;
pub mod routes;
pub mod scheduler;

use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, Utc};
use mz_audit::{ActorType, AuditHandle};
use mz_core::AppState;
use mz_core::time::now_naive;
use mz_jobs::{CronJob, Schedule};
use mz_paypal::PaypalClient;

pub use mail::Mailer;
pub use routes::router;

/// Dependensi satu run cron / satu request force.
#[derive(Clone)]
pub struct Ctx {
    pub state: AppState,
    pub paypal: PaypalClient,
    pub mail: Mailer,
    /// Pelaku audit: `runAs(system, <cron>)` untuk cron, handle request (operator) untuk route.
    pub audit: AuditHandle,
    /// `new Date()` run ini (UTC, presisi milidetik seperti `timestamp(3)`).
    pub now: NaiveDateTime,
}

impl Ctx {
    /// Cron: `requestContext.runAs({ actorType: 'system', actorName })`.
    pub fn cron(state: &AppState, actor_name: &str) -> Self {
        Self::request(
            state,
            AuditHandle::run_as(state.db.clone(), ActorType::System, Some(actor_name)),
        )
    }

    /// Route operator: konteks request (operator) diteruskan apa adanya, seperti `runAs` Node
    /// yang tidak menimpa konteks yang sudah ada.
    pub fn request(state: &AppState, audit: AuditHandle) -> Self {
        Self {
            state: state.clone(),
            paypal: PaypalClient::from_state(state),
            mail: Mailer::Live,
            audit,
            now: now_naive(),
        }
    }

    /// Ganti jam (dipotong ke milidetik).
    #[must_use]
    pub fn at(mut self, now: NaiveDateTime) -> Self {
        self.now = DateTime::<Utc>::from_timestamp_millis(now.and_utc().timestamp_millis())
            .map_or(now, |d| d.naive_utc());
        self
    }

    pub fn now_utc(&self) -> DateTime<Utc> {
        self.now.and_utc()
    }
}

/// Delapan `@Cron(..., { timeZone: CRON_TIMEZONE })` billing. Mati bawaan sampai disebut di `MZ_JOBS`.
pub fn cron_jobs() -> Vec<CronJob> {
    const HOUR: Duration = Duration::from_secs(3600);
    vec![
        // tasks.service.ts:402 `@Cron('55 21 * * *')`
        CronJob {
            name: "payment-failure-escalation",
            schedule: Schedule::daily(&[21], &[55]),
            timeout: HOUR,
            run: |s| {
                Box::pin(async move {
                    escalation::run(&Ctx::cron(&s, escalation::ACTOR)).await;
                    Ok(())
                })
            },
        },
        // :574 `@Cron('50 21 * * *')`
        CronJob {
            name: "download-reminder",
            schedule: Schedule::daily(&[21], &[50]),
            timeout: HOUR,
            run: |s| {
                Box::pin(async move {
                    reminders::download_reminder(&Ctx::cron(&s, "download-reminder")).await;
                    Ok(())
                })
            },
        },
        // :662 `@Cron('45 21 * * *')`
        CronJob {
            name: "trial-ending-reminder",
            schedule: Schedule::daily(&[21], &[45]),
            timeout: HOUR,
            run: |s| {
                Box::pin(async move {
                    reminders::trial_ending_reminder(&Ctx::cron(&s, "trial-ending-reminder")).await;
                    Ok(())
                })
            },
        },
        // :711 `@Cron('45 21 * * *')`
        CronJob {
            name: "trial-ended-reminder",
            schedule: Schedule::daily(&[21], &[45]),
            timeout: HOUR,
            run: |s| {
                Box::pin(async move {
                    reminders::trial_ended_reminder(&Ctx::cron(&s, "trial-ended-reminder")).await;
                    Ok(())
                })
            },
        },
        // :772 `@Cron('50 21 * * *')`
        CronJob {
            name: "manual-renewal-reminder",
            schedule: Schedule::daily(&[21], &[50]),
            timeout: HOUR,
            run: |s| {
                Box::pin(async move {
                    reminders::manual_renewal_reminder(&Ctx::cron(&s, "manual-renewal-reminder"))
                        .await;
                    Ok(())
                })
            },
        },
        // :864 `@Cron('0 2 * * *')`
        CronJob {
            name: "invoice-scheduler",
            schedule: Schedule::daily(&[2], &[0]),
            timeout: HOUR * 2,
            run: |s| {
                Box::pin(async move {
                    scheduler::invoice_scheduler(&Ctx::cron(&s, "invoice-scheduler")).await;
                    Ok(())
                })
            },
        },
        // :2027 `@Cron('0 4 * * *')`
        CronJob {
            name: "unpaid-prorated-users-expiration",
            schedule: Schedule::daily(&[4], &[0]),
            timeout: HOUR,
            run: |s| {
                Box::pin(async move {
                    proration_expiry::handle_cron(&Ctx::cron(&s, proration_expiry::ACTOR)).await;
                    Ok(())
                })
            },
        },
        // :2283 `@Cron('0 3 * * *')`
        CronJob {
            name: "subscription-expiration",
            schedule: Schedule::daily(&[3], &[0]),
            timeout: HOUR * 2,
            run: |s| {
                Box::pin(async move {
                    expiration::handle_cron(&Ctx::cron(&s, expiration::ACTOR)).await;
                    Ok(())
                })
            },
        },
    ]
}

/// Pemilik company untuk email billing: `company.userCompany` ber-`isDefault` pertama (+ `user`).
/// Dipakai sebagai `LEFT JOIN LATERAL (...) o ON true` dengan alias subscription `s`.
pub(crate) const OWNER_LATERAL: &str = r#"LEFT JOIN LATERAL (
        SELECT u.username AS owner_email, u.name AS owner_name
          FROM "userCompany" uc JOIN users u ON u.id = uc."userId"
         WHERE uc."companyId" = s."companyId" AND uc."isDefault" = true
         ORDER BY uc.id LIMIT 1) o ON true"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_names_and_schedules() {
        let jobs = cron_jobs();
        let names: Vec<&str> = jobs.iter().map(|j| j.name).collect();
        assert_eq!(
            names,
            [
                "payment-failure-escalation",
                "download-reminder",
                "trial-ending-reminder",
                "trial-ended-reminder",
                "manual-renewal-reminder",
                "invoice-scheduler",
                "unpaid-prorated-users-expiration",
                "subscription-expiration",
            ]
        );
        // 02:00 WIB = 19:00 UTC hari sebelumnya.
        let after = DateTime::parse_from_rfc3339("2026-11-17T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let next = jobs[5].schedule.next_after(after).unwrap();
        assert_eq!(next.to_rfc3339(), "2026-11-17T19:00:00+00:00");
        let next = jobs[0].schedule.next_after(after).unwrap();
        assert_eq!(next.to_rfc3339(), "2026-11-17T14:55:00+00:00");
    }
}
