//! `MailService.sendInvoiceConfirmation` / `sendSubscriptionCancellation` /
//! `sendResumeSubscription` / `sendReactivateSubscription` (`src/mail/mail.service.ts`) sebagai
//! fungsi tipis di atas `mz_core::mail` (template `.hbs` yang sama, `{...payload}` sebagai konteks).

use mz_core::AppState;
use mz_core::jsfmt::num;
use mz_core::mail::OwnedMail;
use serde_json::{Map, Value, json};

/// Payload `sendInvoiceConfirmation` dari `renewPayment` (Midtrans).
pub struct InvoiceConfirmation {
    pub email: String,
    pub name: String,
    pub tier_name: String,
    pub invoice_number: String,
    /// `payment.payment_url` (`undefined` → kunci dibuang).
    pub payment_url: Option<String>,
    pub period: String,
    pub total_price: f64,
    pub expired_at: String,
}

/// Node: mail.service.ts:415 (`sendInvoiceConfirmation`, template `invoice`, kategori `Invoice`).
pub fn invoice_confirmation(p: InvoiceConfirmation) -> OwnedMail {
    let mut ctx = Map::new();
    ctx.insert("email".into(), p.email.clone().into());
    ctx.insert("name".into(), p.name.clone().into());
    ctx.insert("tierName".into(), p.tier_name.into());
    ctx.insert("invoiceNumber".into(), p.invoice_number.clone().into());
    if let Some(u) = p.payment_url {
        ctx.insert("paymentUrl".into(), u.into());
    }
    ctx.insert("period".into(), p.period.into());
    ctx.insert("totalPrice".into(), num(p.total_price));
    ctx.insert("expiredAt".into(), p.expired_at.into());
    OwnedMail {
        to_email: p.email,
        to_name: p.name,
        subject: format!("Timemoz Subscription Invoice – #{}", p.invoice_number),
        text: None,
        template: "invoice",
        context: Value::Object(ctx),
        category: "Invoice",
        label: "sendInvoiceConfirmation",
    }
}

/// `founderName: process.env.MAIL_FOUNDER_NAME` (`undefined` → kunci dibuang).
fn with_founder(state: &AppState, mut ctx: Value) -> Value {
    if let (Some(o), Some(f)) = (ctx.as_object_mut(), state.config.var("MAIL_FOUNDER_NAME")) {
        o.insert("founderName".into(), f.into());
    }
    ctx
}

/// Node: mail.service.ts:531 (`sendSubscriptionCancellation`).
pub fn subscription_cancellation(
    state: &AppState,
    email: &str,
    name: &str,
    tier_name: &str,
    date: &str,
) -> OwnedMail {
    OwnedMail {
        to_email: email.to_owned(),
        to_name: name.to_owned(),
        subject: "Subscription Cancelled – Timemoz".into(),
        text: None,
        template: "subscription-cancellation",
        context: with_founder(
            state,
            json!({ "email": email, "name": name, "tierName": tier_name, "date": date }),
        ),
        category: "Subscription",
        label: "sendSubscriptionCancellation",
    }
}

/// Node: mail.service.ts:1609 (`sendResumeSubscription`).
pub fn resume_subscription(
    state: &AppState,
    email: &str,
    name: &str,
    next_billing_date: &str,
) -> OwnedMail {
    OwnedMail {
        to_email: email.to_owned(),
        to_name: name.to_owned(),
        subject: "Your Timemoz Subscription Has Been Resumed".into(),
        text: None,
        template: "resume-subscription",
        context: with_founder(
            state,
            json!({ "email": email, "name": name, "nextBillingDate": next_billing_date }),
        ),
        category: "Subscription",
        label: "sendResumeSubscription",
    }
}

/// Node: mail.service.ts:1716 (`sendReactivateSubscription`).
pub fn reactivate_subscription(
    state: &AppState,
    email: &str,
    name: &str,
    next_billing_date: &str,
) -> OwnedMail {
    OwnedMail {
        to_email: email.to_owned(),
        to_name: name.to_owned(),
        subject: "Your Timemoz Subscription Has Been Reactivated".into(),
        text: None,
        template: "reactivate-subscription",
        context: with_founder(
            state,
            json!({ "email": email, "name": name, "nextBillingDate": next_billing_date }),
        ),
        category: "Subscription",
        label: "sendReactivateSubscription",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Variabel `{{x}}` di template `.hbs` (akar workspace).
    fn vars(template: &str) -> Vec<String> {
        let path = format!(
            "{}/../../templates/mail/{template}.hbs",
            env!("CARGO_MANIFEST_DIR")
        );
        let src = std::fs::read_to_string(path).unwrap();
        src.split("{{")
            .skip(1)
            .filter_map(|p| p.split("}}").next())
            .filter(|v| !v.starts_with('#') && !v.starts_with('/'))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn surat_invoice() {
        let m = invoice_confirmation(InvoiceConfirmation {
            email: "a@b.co".into(),
            name: "Agus".into(),
            tier_name: "Grow".into(),
            invoice_number: "INV-01012026-1".into(),
            payment_url: Some("https://pay/x".into()),
            period: "01-01-2026 - 31-01-2026".into(),
            total_price: 30.0,
            expired_at: "31-01-2026".into(),
        });
        assert_eq!(m.subject, "Timemoz Subscription Invoice – #INV-01012026-1");
        assert_eq!(m.category, "Invoice");
        assert_eq!(m.context["totalPrice"], json!(30));
        let keys: Vec<&String> = m.context.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "email",
                "name",
                "tierName",
                "invoiceNumber",
                "paymentUrl",
                "period",
                "totalPrice",
                "expiredAt"
            ]
        );
        for v in vars("invoice") {
            assert!(
                v == "paymentDueDate" || m.context.get(&v).is_some(),
                "invoice: {v}"
            );
        }
    }

    fn state_stub(cfg: mz_core::config::Config) -> AppState {
        let mut st = AppState::for_test().unwrap();
        st.config = std::sync::Arc::new(cfg);
        st
    }

    #[tokio::test]
    async fn surat_langganan() {
        let mut cfg = mz_core::config::Config::for_test();
        cfg.var_overrides
            .insert("MAIL_FOUNDER_NAME".into(), "Founder".into());
        let ctx = |m: &OwnedMail| m.context.clone();
        let check = |m: OwnedMail, subject: &str| {
            assert_eq!(m.subject, subject);
            assert_eq!(m.category, "Subscription");
            for v in vars(m.template) {
                assert!(
                    v == "founderName" || ctx(&m).get(&v).is_some(),
                    "{}: {v}",
                    m.template
                );
            }
        };
        let st = state_stub(cfg);
        let c = subscription_cancellation(&st, "a@b.co", "Agus", "Grow", "01-12-2026");
        assert_eq!(c.context["founderName"], "Founder");
        check(c, "Subscription Cancelled – Timemoz");
        check(
            resume_subscription(&st, "a@b.co", "Agus", "01-12-2026"),
            "Your Timemoz Subscription Has Been Resumed",
        );
        check(
            reactivate_subscription(&st, "a@b.co", "Agus", "01-12-2026"),
            "Your Timemoz Subscription Has Been Reactivated",
        );
    }
}
