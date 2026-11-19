//! Inti `src/mail/mail.service.ts`: render template Handlebars (berkas `.hbs`
//! yang SAMA, disalin ke `templates/mail/`) lalu kirim lewat Mailtrap Send API.
//!
//! Setiap `sendXxx` di Node dipindah ke crate domain pemakainya sebagai fungsi
//! tipis di atas [`send`]. Semua `sendXxx` Node menelan error dan mengembalikan
//! `{ success, message }`; [`send`] mengembalikan `MailResult` yang sama
//! supaya handler yang meneruskan nilai itu ke respons tetap identik.

use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use handlebars::Handlebars;
use serde::Serialize;
use serde_json::{Value, json};

use crate::AppState;

const MAILTRAP_URL: &str = "https://send.api.mailtrap.io/api/send";
const MAILTRAP_TIMEOUT: Duration = Duration::from_secs(15);

fn registry() -> &'static RwLock<Handlebars<'static>> {
    static REG: OnceLock<RwLock<Handlebars<'static>>> = OnceLock::new();
    REG.get_or_init(|| {
        let mut h = Handlebars::new();
        // handlebars.js tidak strict secara bawaan (variabel hilang = string kosong).
        h.set_strict_mode(false);
        RwLock::new(h)
    })
}

fn template_dir() -> String {
    std::env::var("MAIL_TEMPLATE_DIR").unwrap_or_else(|_| "templates/mail".into())
}

/// `getHtmlFromTemplate(name, context)` — template di-cache setelah dibaca pertama kali.
pub fn render(template: &str, context: &Value) -> anyhow::Result<String> {
    {
        let reg = registry().read().unwrap_or_else(|e| e.into_inner());
        if reg.has_template(template) {
            return Ok(reg.render(template, context)?);
        }
    }
    let path = std::path::Path::new(&template_dir()).join(format!("{template}.hbs"));
    let source = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("template {} tidak bisa dibaca: {e}", path.display()))?;
    let mut reg = registry().write().unwrap_or_else(|e| e.into_inner());
    reg.register_template_string(template, source)?;
    Ok(reg.render(template, context)?)
}

#[derive(Debug, Clone, Serialize)]
pub struct MailResult {
    pub success: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Satu email Mailtrap dengan bentuk body yang sama dengan Node.
pub struct Mail<'a> {
    pub to_email: &'a str,
    pub to_name: &'a str,
    pub subject: &'a str,
    /// `text` di Node selalu = subject, kecuali disebut lain.
    pub text: Option<&'a str>,
    pub template: &'a str,
    pub context: Value,
    pub category: &'a str,
    /// Nama fungsi Node untuk log & pesan (`send invite email success`).
    pub label: &'a str,
}

/// Satu penerima Mailtrap. `name: None` → objek `{ email }` saja (mis. `sendReportEmail`).
#[derive(Debug, Clone, Copy)]
pub struct Recipient<'a> {
    pub email: &'a str,
    pub name: Option<&'a str>,
}

/// Lampiran Mailtrap (`disposition: attachment`, isi di-base64 saat dikirim).
#[derive(Debug, Clone, Copy)]
pub struct Attachment<'a> {
    pub filename: &'a str,
    /// MIME, mis. `application/pdf`.
    pub mime: &'a str,
    pub content: &'a [u8],
}

/// Email yang HTML-nya sudah dirender. Dipakai langsung oleh pengirim Node yang
/// berbeda dari bentuk [`Mail`] (lampiran, banyak penerima, `text` khusus, timeout lain).
#[derive(Debug, Clone, Copy)]
pub struct Message<'a> {
    pub to: &'a [Recipient<'a>],
    pub subject: &'a str,
    pub text: &'a str,
    pub html: &'a str,
    pub category: &'a str,
    pub attachments: &'a [Attachment<'a>],
    /// Timeout axios di Node (bawaan 15 dtk; `sendReportEmail` 30 dtk).
    pub timeout: Duration,
}

impl Message<'_> {
    pub const DEFAULT_TIMEOUT: Duration = MAILTRAP_TIMEOUT;
}

/// Nama pengirim bawaan `MailService` Node (`from: { name: 'Timemoz' }`).
pub const DEFAULT_FROM_NAME: &str = "Timemoz";

/// Body Mailtrap Send API persis bentuk `MailService` Node. Murni (tanpa jaringan).
pub fn message_body(from_email: Option<&str>, m: &Message<'_>) -> Value {
    message_body_named(from_email, DEFAULT_FROM_NAME, m)
}

/// [`message_body`] dengan nama pengirim lain (mis. `sendQuickstartReminder` = `Timemoz Team`).
pub fn message_body_named(from_email: Option<&str>, from_name: &str, m: &Message<'_>) -> Value {
    use base64::Engine as _;
    let to: Vec<Value> =
        m.to.iter()
            .map(|r| match r.name {
                Some(name) => json!({ "email": r.email, "name": name }),
                None => json!({ "email": r.email }),
            })
            .collect();
    let attachments: Vec<Value> = m
        .attachments
        .iter()
        .map(|a| {
            json!({
                "content": base64::engine::general_purpose::STANDARD.encode(a.content),
                "filename": a.filename,
                "type": a.mime,
                "disposition": "attachment",
            })
        })
        .collect();
    json!({
        "to": to,
        "cc": [],
        "bcc": [],
        "from": { "email": from_email, "name": from_name },
        "attachments": attachments,
        "custom_variables": {},
        "headers": { "X-Message-Source": "timemoz.com" },
        "subject": m.subject,
        "text": m.text,
        "html": m.html,
        "category": m.category,
    })
}

fn failed(label: &str) -> MailResult {
    MailResult {
        success: false,
        message: format!("{label} failed"),
        data: None,
    }
}

/// Kirim [`Message`] ke Mailtrap. `label` = nama fungsi Node untuk log & pesan
/// (`<label> success` / `<label> failed`). Tidak pernah gagal keras, seperti Node.
pub async fn deliver(state: &AppState, label: &str, m: &Message<'_>) -> MailResult {
    deliver_named(state, label, DEFAULT_FROM_NAME, m).await
}

/// [`deliver`] dengan nama pengirim lain.
pub async fn deliver_named(
    state: &AppState,
    label: &str,
    from_name: &str,
    m: &Message<'_>,
) -> MailResult {
    // Tanpa token Mailtrap pasti menolak (401) — hasilnya sama-sama "gagal",
    // jadi jangan keluar ke jaringan (dev/test tidak mengirim apa pun).
    let Some(token) = state.config.var("MAILTRAP_TOKEN") else {
        tracing::warn!(
            function = label,
            "MAILTRAP_TOKEN kosong; email tidak dikirim"
        );
        return failed(label);
    };
    let body = message_body_named(state.config.var("MAIL_FROM").as_deref(), from_name, m);
    let resp = state
        .http
        .post(MAILTRAP_URL)
        .header("Api-Token", token)
        .timeout(m.timeout)
        .json(&body)
        .send()
        .await;
    match resp {
        // axios melempar untuk status ≥ 400 → cabang gagal di Node.
        Ok(r) if r.status().is_success() => {
            let status = r.status().as_u16();
            let data: Option<Value> = r.json().await.ok();
            tracing::info!(function = label, status, "mail sent");
            MailResult {
                success: true,
                message: format!("{label} success"),
                data,
            }
        }
        Ok(r) => {
            tracing::error!(
                function = label,
                status = r.status().as_u16(),
                "mail rejected"
            );
            failed(label)
        }
        Err(err) => {
            tracing::error!(function = label, error = %err, "mail error");
            failed(label)
        }
    }
}

pub async fn send(state: &AppState, mail: Mail<'_>) -> MailResult {
    send_named(state, DEFAULT_FROM_NAME, mail).await
}

/// [`send`] dengan nama pengirim lain.
pub async fn send_named(state: &AppState, from_name: &str, mail: Mail<'_>) -> MailResult {
    let html = match render(mail.template, &mail.context) {
        Ok(h) => h,
        Err(err) => {
            tracing::error!(template = mail.template, error = %err, "mail template error");
            return failed(mail.label);
        }
    };
    let to = [Recipient {
        email: mail.to_email,
        name: Some(mail.to_name),
    }];
    deliver_named(
        state,
        mail.label,
        from_name,
        &Message {
            to: &to,
            subject: mail.subject,
            text: mail.text.unwrap_or(mail.subject),
            html: &html,
            category: mail.category,
            attachments: &[],
            timeout: MAILTRAP_TIMEOUT,
        },
    )
    .await
}

/// Kirim di latar belakang (pengganti `void this.mail.sendX(...)` di Node).
pub fn spawn_send(state: &AppState, mail: OwnedMail) {
    let state = state.clone();
    crate::bg::spawn(async move {
        let _ = send(&state, mail.as_mail()).await;
    });
}

/// Versi `'static` dari [`Mail`] untuk [`spawn_send`].
pub struct OwnedMail {
    pub to_email: String,
    pub to_name: String,
    pub subject: String,
    pub text: Option<String>,
    pub template: &'static str,
    pub context: Value,
    pub category: &'static str,
    pub label: &'static str,
}

impl OwnedMail {
    pub fn as_mail(&self) -> Mail<'_> {
        Mail {
            to_email: &self.to_email,
            to_name: &self.to_name,
            subject: &self.subject,
            text: self.text.as_deref(),
            template: self.template,
            context: self.context.clone(),
            category: self.category,
            label: self.label,
        }
    }
}

/// Supaya `Arc<str>` dkk. bisa dipakai tanpa clone berulang di pemanggil.
pub type SharedStr = Arc<str>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_body_named_sets_sender_name() {
        let to = [Recipient {
            email: "a@x.test",
            name: Some("Ani"),
        }];
        let m = Message {
            to: &to,
            subject: "S",
            text: "T",
            html: "",
            category: "C",
            attachments: &[],
            timeout: Message::DEFAULT_TIMEOUT,
        };
        let body = message_body_named(Some("noreply@x.test"), "Timemoz Team", &m);
        assert_eq!(
            body.get("from"),
            Some(&json!({"email": "noreply@x.test", "name": "Timemoz Team"}))
        );
        assert_eq!(
            message_body(None, &m).get("from"),
            Some(&json!({"email": null, "name": "Timemoz"}))
        );
    }

    #[test]
    fn message_body_shape() {
        let to = [
            Recipient {
                email: "a@x.test",
                name: Some("Ani"),
            },
            Recipient {
                email: "b@x.test",
                name: None,
            },
        ];
        let att = [Attachment {
            filename: "r.pdf",
            mime: "application/pdf",
            content: b"PDF",
        }];
        let body = message_body(
            Some("noreply@x.test"),
            &Message {
                to: &to,
                subject: "S",
                text: "T",
                html: "<p>H</p>",
                category: "C",
                attachments: &att,
                timeout: Message::DEFAULT_TIMEOUT,
            },
        );
        assert_eq!(
            body,
            json!({
                "to": [{"email": "a@x.test", "name": "Ani"}, {"email": "b@x.test"}],
                "cc": [],
                "bcc": [],
                "from": {"email": "noreply@x.test", "name": "Timemoz"},
                "attachments": [{
                    "content": "UERG",
                    "filename": "r.pdf",
                    "type": "application/pdf",
                    "disposition": "attachment",
                }],
                "custom_variables": {},
                "headers": {"X-Message-Source": "timemoz.com"},
                "subject": "S",
                "text": "T",
                "html": "<p>H</p>",
                "category": "C",
            })
        );
    }
}
