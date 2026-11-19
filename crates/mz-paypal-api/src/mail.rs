//! `MailService.sendInvoiceConfirmation` / `sendWelcomeToTheTeam` / `sendPaymentFailed`
//! (`src/mail/mail.service.ts`) sebagai fungsi tipis di atas `mz_core::mail`.

use mz_core::AppState;
use mz_core::jsfmt::num;
use mz_core::mail::{OwnedMail, spawn_send};
use serde_json::{Value, json};

/// Payload `sendInvoiceConfirmation`.
pub struct InvoiceConfirmation {
    pub email: String,
    pub name: String,
    pub tier_name: String,
    pub invoice_number: String,
    pub period: String,
    pub total_price: f64,
    pub expired_at: String,
}

/// Node: mail.service.ts:415 `sendInvoiceConfirmation` (template `invoice`, `{...payload}`).
pub fn invoice_confirmation(p: InvoiceConfirmation) -> OwnedMail {
    let subject = format!("Timemoz Subscription Invoice – #{}", p.invoice_number);
    OwnedMail {
        to_email: p.email.clone(),
        to_name: p.name.clone(),
        subject,
        text: None,
        template: "invoice",
        context: json!({
            "email": p.email,
            "name": p.name,
            "tierName": p.tier_name,
            "invoiceNumber": p.invoice_number,
            "paymentUrl": "",
            "period": p.period,
            "totalPrice": num(p.total_price),
            "expiredAt": p.expired_at,
        }),
        category: "Invoice",
        label: "sendInvoiceConfirmation",
    }
}

/// Node: mail.service.ts:1042 `sendWelcomeToTheTeam`.
pub fn welcome_to_the_team(email: &str, name: &str) -> OwnedMail {
    OwnedMail {
        to_email: email.to_owned(),
        to_name: name.to_owned(),
        subject: "You're in! Welcome to the Timemoz community.".into(),
        text: None,
        template: "welcome-to-the-team",
        context: json!({ "name": name }),
        category: "Welcome",
        label: "sendWelcomeToTheTeam",
    }
}

/// Node: mail.service.ts:896 `sendPaymentFailed` (`url: process.env.PAYMENT_URL`).
pub fn payment_failed(state: &AppState, email: &str, name: &str) -> OwnedMail {
    OwnedMail {
        to_email: email.to_owned(),
        to_name: name.to_owned(),
        subject: "Action Required: We couldn’t process your Timemoz payment".into(),
        text: None,
        template: "payment-failed",
        context: json!({ "url": state.config.var("PAYMENT_URL").map_or(Value::Null, Value::String) }),
        category: "Payment Failed",
        label: "sendPaymentFailed",
    }
}

/// `this.mail.sendX(...).catch(log)` — fire-and-forget.
pub fn send(state: &AppState, mail: OwnedMail) {
    spawn_send(state, mail);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invoice_mail_shape() {
        let m = invoice_confirmation(InvoiceConfirmation {
            email: "a@b.c".into(),
            name: "A".into(),
            tier_name: "3 user(s) @ $5/user".into(),
            invoice_number: "INV-1".into(),
            period: "01-01-2026 - 31-01-2026".into(),
            total_price: 15.0,
            expired_at: "31-01-2026".into(),
        });
        assert_eq!(m.subject, "Timemoz Subscription Invoice – #INV-1");
        assert_eq!(m.context.get("totalPrice"), Some(&json!(15)));
        assert_eq!(m.context.get("paymentUrl"), Some(&json!("")));
        assert_eq!(m.category, "Invoice");
        let html = mz_core::mail::render("invoice", &m.context);
        // Template dibaca relatif ke root workspace; abaikan bila dijalankan dari crate.
        if let Ok(h) = html {
            assert!(h.contains("INV-1"));
        }
    }

    #[test]
    fn welcome_mail_shape() {
        let m = welcome_to_the_team("a@b.c", "A");
        assert_eq!(m.subject, "You're in! Welcome to the Timemoz community.");
        assert_eq!(m.template, "welcome-to-the-team");
    }
}
