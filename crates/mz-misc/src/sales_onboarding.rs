//! Port `src/sales-onboarding-email/*` (service + cron `25 * * * *`): email
//! `sales-trial-onboarding`, dikirim SEKALI:
//! - hanya untuk user yang register dengan paket SALES (`salesOnboardingEmailEligible`,
//!   ditandai registrasi di `mz-auth`: `service/register.rs` dan `service/oauth.rs`),
//! - 24 jam – 7 hari setelah `emailVerifiedAt`,
//! - dan hanya kalau company-nya belum create organization (`company.hasCreateOrgnanization`).
//!
//! User yang ternyata sudah create organization dicabut kelayakannya supaya tidak dipindai
//! ulang tiap jam. Ketiga `users.updateMany` dicatat ke audit seperti middleware Prisma Node
//! (tanpa konteks request = `system`); kolom yang diubah di luar allowlist `users`, jadi
//! normalnya tidak menghasilkan baris.

use std::future::Future;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use chrono::{Duration as CDuration, NaiveDateTime};
use mz_audit::{AuditHandle, Lookup, Model, Row};
use mz_core::AppState;
use mz_core::mail::{Mail, send};
use mz_core::time::{JsDate, now_naive};
use mz_core::user_status::STATUS_ACTIVE;
use mz_jobs::{CronJob, Schedule};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::demo_requests::first_name_of;
use crate::{Mailtrap, RunGuard};

const HOUR_MS: i64 = 60 * 60 * 1000;

/// `SALES_ONBOARDING_DELAY_MS`: jeda setelah verifikasi email sebelum email dikirim.
pub const SALES_ONBOARDING_DELAY_MS: i64 = 24 * HOUR_MS;

/// `SALES_ONBOARDING_MAX_AGE_MS`: batas atas umur verifikasi (email "hari pertama" yang
/// datang seminggu kemudian sudah tidak relevan).
pub const SALES_ONBOARDING_MAX_AGE_MS: i64 = 7 * 24 * HOUR_MS;

/// `BATCH_SIZE`.
pub const BATCH_SIZE: i64 = 100;

/// Node: src/mail/mail.service.ts:20 `SALES_TRIAL_WHATSAPP_URL` (konstanta, bukan input).
pub const SALES_TRIAL_WHATSAPP_URL: &str = "https://wa.me/628218051703?text=Halo%2C%20saya%20baru%20registrasi%20Timemoz%20dan%20butuh%20bantuan%20setup";

/// `SalesOnboardingSweepResult`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepResult {
    pub sent: u32,
    pub skipped_onboarded: u32,
    pub failed: u32,
}

/// Kandidat (`select: { id, username, name }`).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Candidate {
    pub id: i32,
    pub username: String,
    pub name: String,
}

/// Jendela `emailVerifiedAt`: `(lte, gte)` = `(now - 24 jam, now - 7 hari)`.
pub fn verified_window(now: NaiveDateTime) -> (NaiveDateTime, NaiveDateTime) {
    (
        now - CDuration::milliseconds(SALES_ONBOARDING_DELAY_MS),
        now - CDuration::milliseconds(SALES_ONBOARDING_MAX_AGE_MS),
    )
}

/// Pengirim `MailService.sendSalesTrialOnboarding({ email, name })`. Hasil = `sent.success`.
/// Produksi: [`Mailtrap`]; uji: tiruan tanpa jaringan.
pub trait OnboardingMailer: Sync {
    fn send_onboarding(&self, email: &str, name: &str) -> impl Future<Output = bool> + Send;
}

/// Konteks Handlebars `sales-trial-onboarding.hbs`.
pub fn onboarding_mail_context(name: &str) -> Value {
    json!({
        "firstName": first_name_of(name),
        "whatsappUrl": SALES_TRIAL_WHATSAPP_URL,
    })
}

impl OnboardingMailer for Mailtrap<'_> {
    /// Node: src/mail/mail.service.ts:1329 `sendSalesTrialOnboarding`.
    async fn send_onboarding(&self, email: &str, name: &str) -> bool {
        send(
            self.0,
            Mail {
                to_email: email,
                to_name: name,
                subject: "Akun Anda Sudah Aktif. 3 langkah agar tim bisa langsung jalan",
                text: Some("Setup geofence pertama cuma butuh 5 menit."),
                template: "sales-trial-onboarding",
                context: onboarding_mail_context(name),
                category: "Sales Trial Onboarding",
                label: "send sales trial onboarding email",
            },
        )
        .await
        .success
    }
}

/// Node: src/sales-onboarding-email/sales-onboarding-email.service.ts:48 `runSweep(now)`.
/// Kandidat dipindai per 100 (kursor `id`); galat satu user tidak menghentikan sisa batch.
/// Galat query kandidat diteruskan ke pemanggil.
pub async fn run_sweep<M: OnboardingMailer>(
    db: &PgPool,
    now: NaiveDateTime,
    mailer: &M,
) -> sqlx::Result<SweepResult> {
    let audit = AuditHandle::detached(db.clone());
    let mut result = SweepResult::default();
    let (verified_before, verified_after) = verified_window(now);
    let mut cursor_id: i32 = 0;
    loop {
        let candidates: Vec<Candidate> = sqlx::query_as(
            r#"SELECT id, username, name FROM users
               WHERE id > $1
                 AND "salesOnboardingEmailEligible" = true
                 AND "salesOnboardingEmailSentAt" IS NULL
                 AND status = $2
                 AND "emailVerifiedAt" <= $3
                 AND "emailVerifiedAt" >= $4
               ORDER BY id ASC
               LIMIT $5"#,
        )
        .bind(cursor_id)
        .bind(STATUS_ACTIVE)
        .bind(verified_before)
        .bind(verified_after)
        .bind(BATCH_SIZE)
        .fetch_all(db)
        .await?;

        for user in &candidates {
            if let Err(e) = process_user(db, &audit, mailer, user, now, &mut result).await {
                // Satu user rusak tidak boleh menghentikan sisa batch.
                result.failed += 1;
                tracing::error!(
                    "sales onboarding email: user {} gagal diproses: {e}",
                    user.id
                );
            }
        }

        let Some(last) = candidates.last().map(|c| c.id) else {
            break;
        };
        if (candidates.len() as i64) < BATCH_SIZE {
            break;
        }
        cursor_id = last;
    }

    if result.sent > 0 || result.skipped_onboarded > 0 || result.failed > 0 {
        tracing::info!(
            "sales onboarding email: sent={} skippedOnboarded={} failed={}",
            result.sent,
            result.skipped_onboarded,
            result.failed
        );
    }
    Ok(result)
}

/// `userCompany.findFirst({ ...userCompanyScope(userId), select: { company: { select:
/// { hasCreateOrgnanization } } } })` → `membership?.company?.hasCreateOrgnanization`.
async fn has_created_organization(db: &PgPool, user_id: i32) -> sqlx::Result<bool> {
    let flag: Option<Option<bool>> = sqlx::query_scalar(
        r#"SELECT c."hasCreateOrgnanization"
           FROM "userCompany" uc
           LEFT JOIN company c ON c.id = uc."companyId"
           WHERE uc."userId" = $1 AND uc.status = 1
           ORDER BY uc."isDefault" DESC, uc.id ASC
           LIMIT 1"#,
    )
    .bind(user_id)
    .fetch_optional(db)
    .await?;
    Ok(flag.flatten().unwrap_or(false))
}

/// Node: src/sales-onboarding-email/sales-onboarding-email.service.ts:98 `processUser`.
async fn process_user<M: OnboardingMailer>(
    db: &PgPool,
    audit: &AuditHandle,
    mailer: &M,
    user: &Candidate,
    now: NaiveDateTime,
    result: &mut SweepResult,
) -> sqlx::Result<()> {
    // Company yang sama dengan yang di-update `createOrganization`.
    if has_created_organization(db, user.id).await? {
        revoke_eligibility(db, audit, user.id).await?;
        result.skipped_onboarded += 1;
        return Ok(());
    }

    // Klaim dulu, kirim kemudian: update bersyarat ini memastikan hanya satu replika yang
    // mengirim. TOCTOU yang tersisa disengaja (sama seperti Node): user bisa create
    // organization di antara cek di atas dan pengiriman.
    if !claim(db, audit, user.id, now).await? {
        return Ok(());
    }

    if !mailer.send_onboarding(&user.username, &user.name).await {
        // Lepas klaim supaya dicoba lagi tick berikutnya (sampai batas umur). Bersyarat pada
        // `now` milik kita supaya tidak melepas klaim replika lain.
        release_claim(db, audit, user.id, now).await?;
        result.failed += 1;
        return Ok(());
    }

    result.sent += 1;
    Ok(())
}

/// `users.updateMany({ where: { id, salesOnboardingEmailSentAt: null },
/// data: { salesOnboardingEmailEligible: false } })`.
pub async fn revoke_eligibility(
    db: &PgPool,
    audit: &AuditHandle,
    user_id: i32,
) -> sqlx::Result<u64> {
    let pre = audit
        .pre_read_many(
            Model::Users,
            Lookup::filter(move |qb| {
                qb.push("id = ").push_bind(user_id);
                qb.push(r#" AND "salesOnboardingEmailSentAt" IS NULL"#);
            }),
        )
        .await;
    let affected = sqlx::query(
        r#"UPDATE users SET "salesOnboardingEmailEligible" = false
           WHERE id = $1 AND "salesOnboardingEmailSentAt" IS NULL"#,
    )
    .bind(user_id)
    .execute(db)
    .await?
    .rows_affected();
    audit
        .record_update_many(
            Model::Users,
            pre,
            affected,
            &Row::new().with("salesOnboardingEmailEligible", false),
            json!({ "id": user_id, "salesOnboardingEmailSentAt": null }),
        )
        .await;
    Ok(affected)
}

/// Klaim antar-replika: `users.updateMany({ where: { id, salesOnboardingEmailEligible: true,
/// salesOnboardingEmailSentAt: null }, data: { salesOnboardingEmailSentAt: now } })`.
/// `true` = klaim milik pemanggil.
pub async fn claim(
    db: &PgPool,
    audit: &AuditHandle,
    user_id: i32,
    now: NaiveDateTime,
) -> sqlx::Result<bool> {
    let pre = audit
        .pre_read_many(
            Model::Users,
            Lookup::filter(move |qb| {
                qb.push("id = ").push_bind(user_id);
                qb.push(r#" AND "salesOnboardingEmailEligible" = true"#);
                qb.push(r#" AND "salesOnboardingEmailSentAt" IS NULL"#);
            }),
        )
        .await;
    let affected = sqlx::query(
        r#"UPDATE users SET "salesOnboardingEmailSentAt" = $2
           WHERE id = $1
             AND "salesOnboardingEmailEligible" = true
             AND "salesOnboardingEmailSentAt" IS NULL"#,
    )
    .bind(user_id)
    .bind(now)
    .execute(db)
    .await?
    .rows_affected();
    audit
        .record_update_many(
            Model::Users,
            pre,
            affected,
            &Row::new().with("salesOnboardingEmailSentAt", json!(JsDate(now))),
            json!({
                "id": user_id,
                "salesOnboardingEmailEligible": true,
                "salesOnboardingEmailSentAt": null,
            }),
        )
        .await;
    Ok(affected > 0)
}

/// `users.updateMany({ where: { id, salesOnboardingEmailSentAt: now },
/// data: { salesOnboardingEmailSentAt: null } })`.
async fn release_claim(
    db: &PgPool,
    audit: &AuditHandle,
    user_id: i32,
    now: NaiveDateTime,
) -> sqlx::Result<u64> {
    let pre = audit
        .pre_read_many(
            Model::Users,
            Lookup::filter(move |qb| {
                qb.push("id = ").push_bind(user_id);
                qb.push(r#" AND "salesOnboardingEmailSentAt" = "#)
                    .push_bind(now);
            }),
        )
        .await;
    let affected = sqlx::query(
        r#"UPDATE users SET "salesOnboardingEmailSentAt" = NULL
           WHERE id = $1 AND "salesOnboardingEmailSentAt" = $2"#,
    )
    .bind(user_id)
    .bind(now)
    .execute(db)
    .await?
    .rows_affected();
    audit
        .record_update_many(
            Model::Users,
            pre,
            affected,
            &Row::new().with("salesOnboardingEmailSentAt", Value::Null),
            json!({ "id": user_id, "salesOnboardingEmailSentAt": JsDate(now) }),
        )
        .await;
    Ok(affected)
}

// ───────────────────────── cron ─────────────────────────

/// `SalesOnboardingEmailCron.running` (hanya dalam satu proses).
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Batas satu jalan (Node tanpa batas).
const SWEEP_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Nama `MZ_JOBS`.
pub const JOB_NAME: &str = "sales-onboarding-email";

/// Node: src/sales-onboarding-email/sales-onboarding-email.cron.ts:19 `@Cron('25 * * * *')`.
pub fn cron_job() -> CronJob {
    CronJob {
        name: JOB_NAME,
        schedule: Schedule::hourly(&[25]),
        timeout: SWEEP_TIMEOUT,
        run: |state| Box::pin(handle_hourly_sweep(state)),
    }
}

/// Node: src/sales-onboarding-email/sales-onboarding-email.cron.ts:19 `handleHourlySweep`.
/// Tick yang bertumpuk dilewati; galat sweep hanya dilog (tidak pernah gagal).
pub async fn handle_hourly_sweep(state: AppState) -> anyhow::Result<()> {
    let Some(_guard) = RunGuard::try_begin(&RUNNING) else {
        tracing::warn!("previous sales onboarding sweep still running, skipping tick");
        return Ok(());
    };
    if let Err(e) = run_sweep(&state.db, now_naive(), &Mailtrap(&state)).await {
        tracing::error!("sales onboarding sweep failed: {e}");
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn now() -> NaiveDateTime {
        NaiveDateTime::parse_from_str("2026-09-15 10:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
    }

    /// sales-onboarding-email.service.spec.ts:44 — jendela 24 jam – 7 hari.
    #[test]
    fn window_is_24h_to_7d_before_now() {
        let (lte, gte) = verified_window(now());
        assert_eq!(lte.to_string(), "2026-09-14 10:00:00");
        assert_eq!(gte.to_string(), "2026-09-08 10:00:00");
    }

    #[test]
    fn mail_context_uses_first_name_and_constant_whatsapp() {
        let v = onboarding_mail_context("  Yudha Pratama ");
        assert_eq!(v["firstName"], "Yudha");
        assert_eq!(v["whatsappUrl"], SALES_TRIAL_WHATSAPP_URL);
        assert_eq!(onboarding_mail_context("")["firstName"], "Tim");
    }

    #[test]
    fn template_renders() {
        let html = crate::test_support::render_template(
            "sales-trial-onboarding",
            &onboarding_mail_context("Yudha"),
        );
        assert!(html.contains("Yudha"));
        assert!(html.contains("https://wa.me/628218051703?text=Halo%2C%20saya%20baru"));
    }

    #[test]
    fn cron_registration() {
        let job = cron_job();
        assert_eq!(job.name, "sales-onboarding-email");
        assert_eq!(job.schedule.minutes, &[25]);
        assert!(job.schedule.hours.is_none());
    }

    #[test]
    fn guard_skips_overlapping_tick_and_releases() {
        let g = RunGuard::try_begin(&RUNNING);
        assert!(g.is_some());
        assert!(RunGuard::try_begin(&RUNNING).is_none());
        drop(g);
        assert!(RunGuard::try_begin(&RUNNING).is_some());
    }
}
