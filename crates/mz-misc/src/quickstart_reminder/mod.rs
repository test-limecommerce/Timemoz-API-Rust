//! Port `src/quickstart-reminder/*` (service + cron `20 * * * *`): email pengingat
//! quickstart, dikirim SEKALI per jenis per company ke owner dan admin company itu.
//!
//! Syarat company: `companyQuickstart.reminderEligible`, modul Sales Tracker aktif, dan
//! subscription terakhirnya berpaket `productType = SALES`. Jejak kirim (sekaligus klaim
//! antar-replika) ada di `companyQuickstartEmail` (unique `companyId, type`). Tidak ada
//! model ber-audit yang ditulis di sini (`companyQuickstart*` di luar cakupan audit).

pub mod content;

use std::future::Future;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use chrono::NaiveDateTime;
use mz_core::AppState;
use mz_core::mail::{Mail, send_named};

/// Nama pengirim `sendQuickstartReminder` (Node `mail.service.ts:1442`).
pub const QUICKSTART_FROM_NAME: &str = "Timemoz Team";
use mz_core::user_status::STATUS_ACTIVE;
use mz_jobs::{CronJob, Schedule};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::demo_requests::first_name_of;
use crate::quickstart::{evaluate_company, percent};
use crate::{Mailtrap, RunGuard};
pub use content::{REMINDER_CHAIN, ReminderContent, ReminderType, StampColumn, reminder_content};

const HOUR_MS: i64 = 60 * 60 * 1000;

/// `QUICKSTART_REMINDER_DELAY_MS`: jeda sejak langkah sebelumnya (atau lahirnya quickstart).
pub const QUICKSTART_REMINDER_DELAY_MS: i64 = 24 * HOUR_MS;

/// `QUICKSTART_REMINDER_MAX_ATTEMPTS`: satu jenis dicoba ulang paling banyak sekian kali.
pub const QUICKSTART_REMINDER_MAX_ATTEMPTS: i32 = 3;

/// `BATCH_SIZE`.
pub const BATCH_SIZE: i64 = 100;

/// `ACTIVE_MEMBERSHIP` (`userCompany.status`).
const ACTIVE_MEMBERSHIP: i32 = 1;

/// `RECIPIENT_ROLE_CODES`: owner (SUPER_ADMIN saat register) + admin lain.
const RECIPIENT_ROLE_CODES: [&str; 2] = ["SUPER_ADMIN", "ADMIN"];

/// `QuickstartStampRow`: stempel `companyQuickstart` + jam database (`dbNow`). Stempel
/// ditulis dengan `NOW()` DB, jadi dibandingkan dengan jam yang sama.
#[derive(Debug, Clone, Default, sqlx::FromRow)]
#[sqlx(rename_all = "camelCase")]
pub struct StampRow {
    pub invite_team_at: Option<NaiveDateTime>,
    pub company_setting_at: Option<NaiveDateTime>,
    pub manage_region_at: Option<NaiveDateTime>,
    pub manage_customer_at: Option<NaiveDateTime>,
    pub manage_store_at: Option<NaiveDateTime>,
    pub visit_plan_at: Option<NaiveDateTime>,
    pub finish_at: Option<NaiveDateTime>,
    pub completed_at: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
    pub db_now: NaiveDateTime,
}

impl StampRow {
    fn stamp(&self, col: StampColumn) -> Option<NaiveDateTime> {
        match col {
            StampColumn::CompanySetting => self.company_setting_at,
            StampColumn::ManageRegion => self.manage_region_at,
            StampColumn::ManageCustomer => self.manage_customer_at,
            StampColumn::ManageStore => self.manage_store_at,
            StampColumn::VisitPlan => self.visit_plan_at,
        }
    }

    /// `QUICKSTART_STEPS.filter((s) => row[s.column] !== null).length` (7 langkah).
    pub fn done_count(&self) -> usize {
        [
            self.invite_team_at,
            self.company_setting_at,
            self.manage_region_at,
            self.manage_customer_at,
            self.manage_store_at,
            self.visit_plan_at,
            self.finish_at,
        ]
        .iter()
        .filter(|s| s.is_some())
        .count()
    }
}

/// Node: src/quickstart-reminder/quickstart-reminder.service.ts:63 `pickDueReminder`.
///
/// - Quickstart selesai (`completedAt`) → APP_INSTALL, langsung.
/// - Selain itu: langkah pertama di [`REMINDER_CHAIN`] yang belum dicentang, jatuh 24 jam
///   setelah stempel langkah sebelumnya (langkah pertama: `createdAt`).
///
/// Hanya SATU kandidat per company, jadi pengingat tidak bisa melangkahi urutan.
pub fn pick_due_reminder(row: &StampRow, now: NaiveDateTime) -> Option<ReminderType> {
    if row.completed_at.is_some() {
        return Some(ReminderType::AppInstall);
    }
    let mut anchor = row.created_at;
    for (kind, col) in REMINDER_CHAIN {
        match row.stamp(col) {
            None => {
                let elapsed =
                    now.and_utc().timestamp_millis() - anchor.and_utc().timestamp_millis();
                return (elapsed >= QUICKSTART_REMINDER_DELAY_MS).then_some(kind);
            }
            Some(done_at) => anchor = done_at,
        }
    }
    // Kelima langkah beres tapi FINISH belum ditekan: belum 100%, belum ada email.
    None
}

/// `QuickstartReminderSweepResult`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepResult {
    pub sent: u32,
    pub failed: u32,
}

/// Penerima: anggota aktif ber-role bawaan SUPER_ADMIN/ADMIN.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
#[sqlx(rename_all = "camelCase")]
pub struct Recipient {
    pub user_id: i32,
    pub email: String,
    pub name: String,
}

/// Satu entri `recipients` (`{ userId, email, success }`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Outcome<'a> {
    user_id: i32,
    email: &'a str,
    success: bool,
}

/// Pengirim email pengingat (`MailService.sendQuickstartReminder`). Hasil = `sent.success`.
/// Produksi: [`Mailtrap`]; uji: tiruan tanpa jaringan.
pub trait ReminderMailer: Sync {
    fn send_reminder(
        &self,
        to: &Recipient,
        content: &ReminderContent,
    ) -> impl Future<Output = bool> + Send;
}

/// Env yang dipakai `sendQuickstartReminder` (kosong = `None`).
#[derive(Debug, Clone, Default)]
pub struct ReminderEnv {
    pub host_fe: Option<String>,
    pub mobile_app_download_url: Option<String>,
    pub wa_support_number: Option<String>,
}

/// Konteks Handlebars `quickstart-reminder.hbs`.
/// Node: src/mail/mail.service.ts:1389 `sendQuickstartReminder` — URL dirakit dari env:
/// `HOST_FE` (CTA dashboard), `MOBILE_APP_DOWNLOAD_URL` (CTA APP_INSTALL),
/// `WA_SUPPORT_NUMBER` (CTA WhatsApp). Env kosong = CTA/baris terkait dilewati.
pub fn reminder_mail_context(name: &str, content: &ReminderContent, env: &ReminderEnv) -> Value {
    let wa_support_number = env.wa_support_number.as_deref().unwrap_or("").trim();
    let wa_digits: String = wa_support_number
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    let download_url = env.mobile_app_download_url.as_deref().unwrap_or("").trim();
    let host_fe = env.host_fe.as_deref().unwrap_or("").trim_end_matches('/');
    let cta_url = if content.cta_prefers_download_link && !download_url.is_empty() {
        download_url.to_owned()
    } else {
        format!("{host_fe}{}", content.cta_path)
    };
    let whatsapp_url = if wa_digits.is_empty() {
        String::new()
    } else {
        format!("https://wa.me/{wa_digits}")
    };
    json!({
        "title": content.text,
        "preheader": content.preheader,
        "firstName": first_name_of(name),
        "paragraphs": content.paragraphs,
        "steps": content.steps,
        "closing": content.closing,
        "notice": content.notice,
        "waSupportNumber": wa_support_number,
        "whatsappUrl": whatsapp_url,
        "ctaLabel": content.cta_label,
        "ctaUrl": cta_url,
    })
}

impl ReminderMailer for Mailtrap<'_> {
    /// Node: src/mail/mail.service.ts:1389 `sendQuickstartReminder`.
    async fn send_reminder(&self, to: &Recipient, content: &ReminderContent) -> bool {
        let state = self.0;
        let env = ReminderEnv {
            host_fe: state.config.var("HOST_FE"),
            mobile_app_download_url: state.config.var("MOBILE_APP_DOWNLOAD_URL"),
            wa_support_number: state.config.var("WA_SUPPORT_NUMBER"),
        };
        // Node `mail.service.ts:1442`: `from: { name: 'Timemoz Team' }` (bukan `Timemoz`).
        send_named(
            state,
            QUICKSTART_FROM_NAME,
            Mail {
                to_email: &to.email,
                to_name: &to.name,
                subject: content.subject,
                text: Some(content.text),
                template: "quickstart-reminder",
                context: reminder_mail_context(&to.name, content, &env),
                category: content.category,
                label: "send quickstart reminder email",
            },
        )
        .await
        .success
    }
}

/// Node: src/quickstart-reminder/quickstart-reminder.service.ts:102 `runSweep`.
/// Kandidat dipindai per 100 (kursor `companyId`); galat satu company tidak menghentikan
/// sisa batch (dihitung `failed`). Galat query kandidat diteruskan ke pemanggil.
pub async fn run_sweep<M: ReminderMailer>(db: &PgPool, mailer: &M) -> sqlx::Result<SweepResult> {
    let mut result = SweepResult::default();
    let mut cursor_id: i32 = 0;
    loop {
        let candidates: Vec<i32> = sqlx::query_scalar(
            r#"SELECT q."companyId"
               FROM "companyQuickstart" q
               JOIN "company" c ON c."id" = q."companyId"
               WHERE q."companyId" > $1
                 AND q."reminderEligible" = true
                 AND c."moduleSalesTrackerEnabled" = true
                 AND NOT EXISTS (
                   SELECT 1 FROM "companyQuickstartEmail" e
                   WHERE e."companyId" = q."companyId"
                     AND e."type" = 'APP_INSTALL'::"QuickstartReminderType"
                     AND e."status" = 'SENT'
                 )
                 AND (
                   SELECT t."productType"::text
                   FROM "subscriptions" s
                   JOIN "pricingTiers" t ON t."id" = s."pricingTierId"
                   WHERE s."companyId" = q."companyId"
                   ORDER BY s."id" DESC
                   LIMIT 1
                 ) = 'SALES'
               ORDER BY q."companyId" ASC
               LIMIT $2"#,
        )
        .bind(cursor_id)
        .bind(BATCH_SIZE)
        .fetch_all(db)
        .await?;

        for company_id in &candidates {
            if let Err(e) = process_company(db, mailer, *company_id, &mut result).await {
                // Satu company rusak tidak boleh menghentikan sisa batch.
                result.failed += 1;
                tracing::error!("quickstart reminder: company {company_id} gagal diproses: {e}");
            }
        }

        let Some(last) = candidates.last().copied() else {
            break;
        };
        if (candidates.len() as i64) < BATCH_SIZE {
            break;
        }
        cursor_id = last;
    }

    if result.sent > 0 || result.failed > 0 {
        tracing::info!(
            "quickstart reminder: sent={} failed={}",
            result.sent,
            result.failed
        );
    }
    Ok(result)
}

/// Node: src/quickstart-reminder/quickstart-reminder.service.ts:157 `processCompany`.
async fn process_company<M: ReminderMailer>(
    db: &PgPool,
    mailer: &M,
    company_id: i32,
    result: &mut SweepResult,
) -> sqlx::Result<()> {
    // Stempel biasanya hanya diperbarui saat dashboard memanggil GET status; evaluasi dulu
    // supaya kita tidak mengingatkan langkah yang sebenarnya sudah selesai.
    evaluate_company(db, company_id).await?;

    let row: Option<StampRow> = sqlx::query_as(
        r#"SELECT "inviteTeamAt", "companySettingAt", "manageRegionAt",
                  "manageCustomerAt", "manageStoreAt", "visitPlanAt", "finishAt",
                  "completedAt", "createdAt", NOW()::timestamp(3) AS "dbNow"
           FROM "companyQuickstart"
           WHERE "companyId" = $1"#,
    )
    .bind(company_id)
    .fetch_optional(db)
    .await?;
    let Some(row) = row else {
        return Ok(());
    };

    let Some(kind) = pick_due_reminder(&row, row.db_now) else {
        return Ok(());
    };

    // Klaim dulu, kirim kemudian: INSERT atomik terhadap unique (companyId, type). Baris
    // FAILED boleh diklaim ulang sampai batas percobaan; SENT dan SENDING tidak pernah.
    // TOCTOU yang tersisa disengaja (sama seperti Node): akibat terburuk satu pengingat
    // yang sudah tidak perlu.
    let claimed: Option<i32> = sqlx::query_scalar(
        r#"INSERT INTO "companyQuickstartEmail"
             ("companyId", "type", "status", "attempts", "claimedAt", "updatedAt")
           VALUES ($1, $2::"QuickstartReminderType", 'SENDING', 1, NOW(), NOW())
           ON CONFLICT ("companyId", "type") DO UPDATE
             SET "status" = 'SENDING',
                 "attempts" = "companyQuickstartEmail"."attempts" + 1,
                 "claimedAt" = NOW(),
                 "updatedAt" = NOW()
             WHERE "companyQuickstartEmail"."status" = 'FAILED'
               AND "companyQuickstartEmail"."attempts" < $3
           RETURNING "id""#,
    )
    .bind(company_id)
    .bind(kind.as_str())
    .bind(QUICKSTART_REMINDER_MAX_ATTEMPTS)
    .fetch_optional(db)
    .await?;
    let Some(email_id) = claimed else {
        return Ok(());
    };

    let recipients = find_recipients(db, company_id).await?;
    if recipients.is_empty() {
        mark_failed(db, email_id, &[], "no active owner/admin recipient").await?;
        result.failed += 1;
        return Ok(());
    }

    let content = reminder_content(kind, percent(row.done_count()));
    let mut outcomes = Vec::with_capacity(recipients.len());
    for r in &recipients {
        let success = mailer.send_reminder(r, &content).await;
        outcomes.push(Outcome {
            user_id: r.user_id,
            email: &r.email,
            success,
        });
    }

    // Satu penerima berhasil = jenis ini dianggap terkirim. Mengulang demi penerima yang
    // gagal berarti mengirim dobel ke yang sudah berhasil.
    if outcomes.iter().any(|o| o.success) {
        sqlx::query(
            r#"UPDATE "companyQuickstartEmail"
               SET "status" = 'SENT', "sentAt" = NOW(),
                   "recipients" = $2::jsonb,
                   "lastError" = NULL, "updatedAt" = NOW()
               WHERE "id" = $1 AND "status" = 'SENDING'"#,
        )
        .bind(email_id)
        .bind(outcomes_json(&outcomes))
        .execute(db)
        .await?;
        result.sent += 1;
        return Ok(());
    }

    mark_failed(db, email_id, &outcomes, "all recipients failed").await?;
    result.failed += 1;
    Ok(())
}

/// `JSON.stringify(outcomes)`.
fn outcomes_json(outcomes: &[Outcome<'_>]) -> String {
    serde_json::to_string(outcomes).unwrap_or_else(|_| "[]".to_owned())
}

/// Node: src/quickstart-reminder/quickstart-reminder.service.ts:246 `markFailed`.
async fn mark_failed(
    db: &PgPool,
    email_id: i32,
    outcomes: &[Outcome<'_>],
    reason: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        r#"UPDATE "companyQuickstartEmail"
           SET "status" = 'FAILED',
               "recipients" = $2::jsonb,
               "lastError" = $3, "updatedAt" = NOW()
           WHERE "id" = $1 AND "status" = 'SENDING'"#,
    )
    .bind(email_id)
    .bind(outcomes_json(outcomes))
    .bind(reason)
    .execute(db)
    .await?;
    Ok(())
}

/// Node: src/quickstart-reminder/quickstart-reminder.service.ts:260 `findRecipients`.
/// Anggota aktif company yang ber-role bawaan SUPER_ADMIN/ADMIN di company itu.
pub async fn find_recipients(db: &PgPool, company_id: i32) -> sqlx::Result<Vec<Recipient>> {
    let [owner_code, admin_code] = RECIPIENT_ROLE_CODES;
    sqlx::query_as(
        r#"SELECT DISTINCT u."id" AS "userId", u."username" AS "email", u."name"
           FROM "userCompany" uc
           JOIN "users" u ON u."id" = uc."userId"
           JOIN "userAccessLevel" ual
             ON ual."userId" = uc."userId" AND ual."companyId" = uc."companyId"
           JOIN "accessLevel" al ON al."id" = ual."accessLevelId"
           WHERE uc."companyId" = $1
             AND uc."status" = $2
             AND u."status" = $3
             AND ual."status" = 1
             AND al."companyId" IS NULL
             AND al."code" IN ($4, $5)
           ORDER BY u."id" ASC"#,
    )
    .bind(company_id)
    .bind(ACTIVE_MEMBERSHIP)
    .bind(STATUS_ACTIVE)
    .bind(owner_code)
    .bind(admin_code)
    .fetch_all(db)
    .await
}

// ───────────────────────── cron ─────────────────────────

/// `QuickstartReminderCron.running` (hanya dalam satu proses).
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Batas satu jalan (Node tanpa batas).
const SWEEP_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Nama `MZ_JOBS`.
pub const JOB_NAME: &str = "quickstart-reminder";

/// Node: src/quickstart-reminder/quickstart-reminder.cron.ts:19 `@Cron('20 * * * *')`.
pub fn cron_job() -> CronJob {
    CronJob {
        name: JOB_NAME,
        schedule: Schedule::hourly(&[20]),
        timeout: SWEEP_TIMEOUT,
        run: |state| Box::pin(handle_hourly_sweep(state)),
    }
}

/// Node: src/quickstart-reminder/quickstart-reminder.cron.ts:19 `handleHourlySweep`.
/// Tick yang bertumpuk dilewati; galat sweep hanya dilog (tidak pernah gagal).
pub async fn handle_hourly_sweep(state: AppState) -> anyhow::Result<()> {
    let Some(_guard) = RunGuard::try_begin(&RUNNING) else {
        tracing::warn!("previous quickstart reminder sweep still running, skipping tick");
        return Ok(());
    };
    if let Err(e) = run_sweep(&state.db, &Mailtrap(&state)).await {
        tracing::error!("quickstart reminder sweep failed: {e}");
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
    use chrono::Duration as CDuration;

    const DAY: i64 = QUICKSTART_REMINDER_DELAY_MS;

    fn t0() -> NaiveDateTime {
        NaiveDateTime::parse_from_str("2026-09-29 00:00:00", "%Y-%m-%d %H:%M:%S").unwrap()
    }

    fn at(ms: i64) -> NaiveDateTime {
        t0() + CDuration::milliseconds(ms)
    }

    fn stamps() -> StampRow {
        StampRow {
            created_at: t0(),
            db_now: t0(),
            ..Default::default()
        }
    }

    // ── pickDueReminder (quickstart-reminder.service.spec.ts:41–89) ──

    #[test]
    fn belum_24_jam_belum_ada_email() {
        assert_eq!(pick_due_reminder(&stamps(), at(DAY - 1)), None);
    }

    #[test]
    fn dua_puluh_empat_jam_tanpa_geofence_company_setting() {
        assert_eq!(
            pick_due_reminder(&stamps(), at(DAY)),
            Some(ReminderType::CompanySetting)
        );
    }

    #[test]
    fn region_dihitung_dari_stempel_geofence() {
        let row = StampRow {
            company_setting_at: Some(at(2 * DAY)),
            ..stamps()
        };
        assert_eq!(pick_due_reminder(&row, at(3 * DAY - 1)), None);
        assert_eq!(
            pick_due_reminder(&row, at(3 * DAY)),
            Some(ReminderType::ManageRegion)
        );
    }

    #[test]
    fn langkah_lebih_dulu_tidak_melompati_urutan() {
        let row = StampRow {
            company_setting_at: Some(at(0)),
            manage_region_at: Some(at(0)),
            manage_store_at: Some(at(0)),
            ..stamps()
        };
        assert_eq!(
            pick_due_reminder(&row, at(DAY)),
            Some(ReminderType::ManageCustomer)
        );
    }

    #[test]
    fn lima_langkah_beres_belum_finish_tidak_ada_email() {
        let row = StampRow {
            company_setting_at: Some(at(0)),
            manage_region_at: Some(at(0)),
            manage_customer_at: Some(at(0)),
            manage_store_at: Some(at(0)),
            visit_plan_at: Some(at(0)),
            ..stamps()
        };
        assert_eq!(pick_due_reminder(&row, at(10 * DAY)), None);
    }

    #[test]
    fn selesai_app_install_tanpa_jeda() {
        let row = StampRow {
            completed_at: Some(t0()),
            ..stamps()
        };
        assert_eq!(
            pick_due_reminder(&row, t0()),
            Some(ReminderType::AppInstall)
        );
    }

    #[test]
    fn visit_plan_progress_is_71_percent_after_five_steps() {
        // quickstart-reminder.service.spec.ts:186 — invite + 4 langkah = 5/7 = 71%.
        let row = StampRow {
            invite_team_at: Some(at(0)),
            company_setting_at: Some(at(0)),
            manage_region_at: Some(at(0)),
            manage_customer_at: Some(at(0)),
            manage_store_at: Some(at(0)),
            ..stamps()
        };
        assert_eq!(
            pick_due_reminder(&row, at(DAY)),
            Some(ReminderType::VisitPlan)
        );
        let c = reminder_content(ReminderType::VisitPlan, percent(row.done_count()));
        assert!(c.paragraphs.first().unwrap().contains("71%"));
    }

    // ── sendQuickstartReminder: konteks template ──

    fn env(host: &str, dl: &str, wa: &str) -> ReminderEnv {
        let o = |s: &str| (!s.is_empty()).then(|| s.to_owned());
        ReminderEnv {
            host_fe: o(host),
            mobile_app_download_url: o(dl),
            wa_support_number: o(wa),
        }
    }

    #[test]
    fn context_dashboard_cta_strips_trailing_slashes() {
        let c = reminder_content(ReminderType::ManageStore, 0);
        let v = reminder_mail_context(
            "  Yudha Pratama",
            &c,
            &env("https://app.test//", "https://dl.test", ""),
        );
        assert_eq!(v["ctaUrl"], "https://app.test/stores");
        assert_eq!(v["firstName"], "Yudha");
        assert_eq!(v["title"], "Manage Store Reminder");
        assert_eq!(v["whatsappUrl"], "");
        assert_eq!(v["waSupportNumber"], "");
        assert!(v["steps"].is_null());
        assert!(v["closing"].is_null());
        assert_eq!(v["notice"], c.notice.unwrap());
    }

    #[test]
    fn context_app_install_prefers_download_link_and_whatsapp_digits() {
        let c = reminder_content(ReminderType::AppInstall, 100);
        let v = reminder_mail_context(
            "",
            &c,
            &env(
                "https://app.test",
                " https://dl.test/app ",
                " +62 812-3456 ",
            ),
        );
        assert_eq!(v["ctaUrl"], "https://dl.test/app");
        assert_eq!(v["firstName"], "Tim");
        assert_eq!(v["waSupportNumber"], "+62 812-3456");
        assert_eq!(v["whatsappUrl"], "https://wa.me/628123456");
        assert_eq!(v["steps"].as_array().map(Vec::len), Some(3));
        assert!(v["notice"].is_null());
        // Tanpa link download: kembali ke path dashboard.
        let v = reminder_mail_context("A", &c, &env("https://app.test", "", ""));
        assert_eq!(v["ctaUrl"], "https://app.test/employee");
    }

    #[test]
    fn template_renders_every_type() {
        // `quickstart-reminder.hbs` sama persis dengan Node (src/mail/templates).
        let render = |ctx: &Value| crate::test_support::render_template("quickstart-reminder", ctx);
        for (kind, _) in REMINDER_CHAIN {
            let c = reminder_content(kind, 43);
            let html = render(&reminder_mail_context(
                "Ani",
                &c,
                &env("https://app.test", "", "0812"),
            ));
            assert!(html.contains("Halo Ani."), "{kind:?}");
            assert!(html.contains(c.cta_label), "{kind:?}");
            assert!(html.contains("https://wa.me/0812"), "{kind:?}");
            assert!(!html.contains("<li>"), "{kind:?}");
        }
        let c = reminder_content(ReminderType::AppInstall, 100);
        let html = render(&reminder_mail_context(
            "Ani",
            &c,
            &env("https://app.test", "", ""),
        ));
        assert!(html.contains("<li>Bagikan link download ke semua anggota tim</li>"));
        assert!(html.contains("https://app.test/employee"));
        assert!(!html.contains("Chat via WhatsApp"));
    }

    #[test]
    fn outcomes_serialize_like_node() {
        let o = [Outcome {
            user_id: 1,
            email: "a@x.test",
            success: true,
        }];
        assert_eq!(
            outcomes_json(&o),
            r#"[{"userId":1,"email":"a@x.test","success":true}]"#
        );
        assert_eq!(outcomes_json(&[]), "[]");
    }

    #[test]
    fn cron_registration() {
        let job = cron_job();
        assert_eq!(job.name, "quickstart-reminder");
        assert_eq!(job.schedule.minutes, &[20]);
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
