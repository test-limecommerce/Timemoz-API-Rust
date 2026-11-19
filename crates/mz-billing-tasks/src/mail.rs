//! `MailService.sendXxx` yang dipakai cron billing (`src/mail/mail.service.ts`), di atas
//! `mz_core::mail` (template `.hbs` + bentuk body Mailtrap yang sama).
//!
//! [`Mailer::Recording`] menyimpan email yang akan dikirim tanpa jaringan — dipakai uji
//! (Mailtrap tidak bisa dialihkan ke server tiruan).

use std::sync::{Arc, Mutex};

use mz_core::AppState;
use mz_core::jsfmt::num;
use mz_core::mail::{Mail, MailResult, send};
use serde_json::{Value, json};

const PRICING_URL_SALES: &str = "https://timemoz.com/pricing/sales-tracker";
const PRICING_URL_TIME: &str = "https://timemoz.com/pricing/time-tracker";

/// Satu email siap kirim (setara argumen body Mailtrap Node).
#[derive(Debug, Clone, PartialEq)]
pub struct OutMail {
    pub template: &'static str,
    pub to_email: String,
    pub to_name: String,
    pub subject: String,
    pub category: &'static str,
    /// Nama fungsi Node untuk log (`<label> success|failed`).
    pub label: &'static str,
    pub context: Value,
}

/// Pengirim email cron billing.
#[derive(Clone)]
pub enum Mailer {
    /// Mailtrap sungguhan lewat `mz_core::mail::send`.
    Live,
    /// Hanya dicatat (uji).
    Recording(Arc<Mutex<Vec<OutMail>>>),
}

impl Mailer {
    pub fn recording() -> Self {
        Self::Recording(Arc::default())
    }

    /// Email yang tercatat (kosong untuk [`Mailer::Live`]).
    pub fn sent(&self) -> Vec<OutMail> {
        match self {
            Self::Live => Vec::new(),
            Self::Recording(log) => log.lock().map(|l| l.clone()).unwrap_or_default(),
        }
    }

    /// `await this.mailService.sendX(...)` — tidak pernah gagal keras, seperti Node.
    pub async fn send(&self, state: &AppState, m: OutMail) -> MailResult {
        match self {
            Self::Live => {
                send(
                    state,
                    Mail {
                        to_email: &m.to_email,
                        to_name: &m.to_name,
                        subject: &m.subject,
                        text: None,
                        template: m.template,
                        context: m.context.clone(),
                        category: m.category,
                        label: m.label,
                    },
                )
                .await
            }
            Self::Recording(log) => {
                let label = m.label;
                if let Ok(mut l) = log.lock() {
                    l.push(m);
                }
                MailResult {
                    success: true,
                    message: format!("{label} success"),
                    data: None,
                }
            }
        }
    }

    /// `this.mailService.sendX(...).catch(...)` tanpa `await` (fire-and-forget).
    pub fn fire(&self, state: &AppState, m: OutMail) {
        match self {
            Self::Live => {
                let (state, this) = (state.clone(), self.clone());
                mz_core::bg::spawn(async move {
                    let _ = this.send(&state, m).await;
                });
            }
            Self::Recording(log) => {
                if let Ok(mut l) = log.lock() {
                    l.push(m);
                }
            }
        }
    }
}

/// `sendD1PaymentFailed` (mail.service.ts:944).
pub fn d1_payment_failed(state: &AppState, email: &str, name: &str) -> OutMail {
    OutMail {
        template: "d+1-payment-failed",
        to_email: email.to_owned(),
        to_name: name.to_owned(),
        subject: "We’re still having trouble with your Timemoz payment".to_owned(),
        category: "Payment Failed D+1",
        label: "send d+1 payment failed email",
        context: json!({ "name": name, "url": state.config.var("PAYMENT_URL") }),
    }
}

/// `sendGoodbyeAccessBlocked` (mail.service.ts:993) — konteks template hanya `url`.
pub fn goodbye_access_blocked(state: &AppState, email: &str, name: &str) -> OutMail {
    OutMail {
        template: "goodbye-access-blocked",
        to_email: email.to_owned(),
        to_name: name.to_owned(),
        subject: "We’re sorry to see you go — Your Timemoz access is now paused".to_owned(),
        category: "Access Blocked",
        label: "send goodbye access blocked email",
        context: json!({ "url": state.config.var("PAYMENT_URL") }),
    }
}

/// `sendDownloadReminder` (mail.service.ts:1100).
pub fn download_reminder(state: &AppState, email: &str, name: &str) -> OutMail {
    OutMail {
        template: "download-reminder",
        to_email: email.to_owned(),
        to_name: name.to_owned(),
        subject: "Please Download the Timemoz Client".to_owned(),
        category: "Download Reminder",
        label: "send download reminder email",
        context: json!({ "name": name, "url": state.config.var("DOWNLOAD_URL") }),
    }
}

/// `sendTrialEndingReminder` (mail.service.ts:1159).
pub fn trial_ending_reminder(email: &str, name: &str, product_type: &str) -> OutMail {
    OutMail {
        template: "trial-ending-reminder",
        to_email: email.to_owned(),
        to_name: name.to_owned(),
        subject: "Seneng Pakai Timemoz? 4 Hari Lagi Trial Kamu Habis".to_owned(),
        category: "Trial Ending Reminder",
        label: "send trial ending reminder email",
        context: json!({
            "name": name,
            "url": if product_type == "SALES" { PRICING_URL_SALES } else { PRICING_URL_TIME },
        }),
    }
}

/// `sendTrialEndedReminder` (mail.service.ts:1214).
pub fn trial_ended_reminder(email: &str, name: &str) -> OutMail {
    OutMail {
        template: "trial-ended-reminder",
        to_email: email.to_owned(),
        to_name: name.to_owned(),
        subject: "Trial Timemoz Kamu Habis dalam 3 Hari".to_owned(),
        category: "Trial Ended Reminder",
        label: "send trial ended reminder email",
        context: json!({ "name": name, "url": PRICING_URL_TIME }),
    }
}

/// `singleLine` (mail.service.ts): `[\r\n]+` → spasi, lalu trim.
fn single_line(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut in_break = false;
    for c in v.chars() {
        if c == '\r' || c == '\n' {
            if !in_break {
                out.push(' ');
            }
            in_break = true;
        } else {
            out.push(c);
            in_break = false;
        }
    }
    mz_core::jsfmt::trim(&out).to_owned()
}

/// `sendManualRenewalReminder` (mail.service.ts:2737) → `postLeaveMail` (penerima `Finance`).
pub fn manual_renewal_reminder(
    state: &AppState,
    to: &str,
    company_name: &str,
    company_uuid: &str,
    plan_name: &str,
    end_date: &str,
) -> OutMail {
    let admin_base = state
        .config
        .var("ADMIN_DASHBOARD_URL")
        .map(|b| b.trim_end_matches('/').to_owned())
        .unwrap_or_default();
    let company_name = single_line(company_name);
    let subject = format!("Manual subscription ends in 7 days: {company_name}");
    let admin_url = (!admin_base.is_empty()).then(|| {
        format!(
            "{admin_base}/dashboard/management-company/{}",
            mz_files::encode::encode_uri_component(company_uuid)
        )
    });
    OutMail {
        template: "manual-renewal-reminder",
        to_email: to.to_owned(),
        to_name: "Finance".to_owned(),
        subject: subject.clone(),
        category: "Manual Renewal Reminder",
        label: "send manual renewal reminder email",
        context: json!({
            "subject": subject,
            "companyName": company_name,
            "companyUuid": company_uuid,
            "planName": plan_name,
            "endDate": end_date,
            "adminUrl": admin_url,
        }),
    }
}

/// `sendDueDate` (mail.service.ts:589).
pub fn due_date(
    email: &str,
    name: &str,
    due_date: &str,
    invoice_number: &str,
    total_price: f64,
) -> OutMail {
    OutMail {
        template: "duedate",
        to_email: email.to_owned(),
        to_name: name.to_owned(),
        subject: format!("Timemoz Billing Notice – Due Date {due_date}"),
        category: "Invoice",
        label: "send email confirmation invoice",
        context: json!({
            "email": email,
            "name": name,
            "dueDate": due_date,
            "invoiceNumber": invoice_number,
            "totalPrice": num(total_price),
        }),
    }
}

/// Isi `sendInvoiceConfirmation` (mail.service.ts:415) untuk invoice proration.
pub struct InvoiceConfirmation<'a> {
    pub email: &'a str,
    pub name: &'a str,
    pub tier_name: &'a str,
    pub invoice_number: &'a str,
    pub payment_url: &'a str,
    pub period: &'a str,
    pub total_price: f64,
    pub expired_at: &'a str,
    pub payment_due_date: &'a str,
}

/// `sendInvoiceConfirmation` (mail.service.ts:415).
pub fn invoice_confirmation(p: &InvoiceConfirmation<'_>) -> OutMail {
    OutMail {
        template: "invoice",
        to_email: p.email.to_owned(),
        to_name: p.name.to_owned(),
        subject: format!("Timemoz Subscription Invoice – #{}", p.invoice_number),
        category: "Invoice",
        label: "send email confirmation invoice",
        context: json!({
            "email": p.email,
            "name": p.name,
            "tierName": p.tier_name,
            "invoiceNumber": p.invoice_number,
            "paymentUrl": p.payment_url,
            "period": p.period,
            "totalPrice": num(p.total_price),
            "expiredAt": p.expired_at,
            "paymentDueDate": p.payment_due_date,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_line_like_node() {
        assert_eq!(single_line("  PT A\r\n\nB  "), "PT A B");
        assert_eq!(single_line("PT"), "PT");
    }

    #[test]
    fn due_date_shape() {
        let m = due_date("a@x.co", "A", "01-12-2026", "INV-1", 30.0);
        assert_eq!(m.subject, "Timemoz Billing Notice – Due Date 01-12-2026");
        assert_eq!(m.context["totalPrice"], json!(30));
    }
}
