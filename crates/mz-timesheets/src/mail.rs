//! Port `sendTimesheetApprovalRequest`, `sendTimesheetStatus`, `sendInvoicePaid`
//! (`mail/mail.service.ts:1901,1972,2043`) di atas `mz_core::mail`.

use mz_core::AppState;
use mz_core::mail::{self, Attachment, Message, OwnedMail, Recipient, render, spawn_send};
use serde_json::{Value, json};

use crate::common::fnum;

pub struct ApprovalRequest {
    pub approver_email: String,
    pub approver_name: String,
    pub employee_name: String,
    pub period_start: String,
    pub period_end: String,
    pub pay_period_type: String,
    pub total_hours: f64,
    pub total_amount: Option<f64>,
    pub currency: String,
    pub notes: Option<String>,
    pub approve_url: String,
    pub reject_url: String,
}

/// Fire-and-forget seperti Node (tanpa `await`).
pub fn send_approval_request(state: &AppState, p: ApprovalRequest) {
    let subject = format!("Timesheet Approval Request from {}", p.employee_name);
    spawn_send(
        state,
        OwnedMail {
            to_email: p.approver_email,
            to_name: p.approver_name.clone(),
            subject,
            text: None,
            template: "timesheet-approval-request",
            context: json!({
                "approverName": p.approver_name,
                "employeeName": p.employee_name,
                "periodStart": p.period_start,
                "periodEnd": p.period_end,
                "payPeriodType": p.pay_period_type,
                "totalHours": crate::jsfmt::num(p.total_hours),
                "totalAmount": fnum(p.total_amount),
                "currency": p.currency,
                "notes": p.notes,
                "approveUrl": p.approve_url,
                "rejectUrl": p.reject_url,
            }),
            category: "Timesheet Approval",
            label: "send timesheet approval request email",
        },
    );
}

pub struct StatusMail {
    pub employee_email: String,
    pub employee_name: String,
    pub is_approved: bool,
    pub period_start: String,
    pub period_end: String,
    pub pay_period_type: String,
    pub total_hours: f64,
    pub total_amount: Option<f64>,
    pub currency: String,
    pub approver_name: String,
    pub rejected_reason: Option<String>,
}

pub fn send_status(state: &AppState, p: StatusMail) {
    let label = if p.is_approved {
        "Approved"
    } else {
        "Rejected"
    };
    spawn_send(
        state,
        OwnedMail {
            to_email: p.employee_email,
            to_name: p.employee_name.clone(),
            subject: format!("Your Timesheet Has Been {label}"),
            text: None,
            template: "timesheet-status",
            context: json!({
                "employeeName": p.employee_name,
                "isApproved": p.is_approved,
                "periodStart": p.period_start,
                "periodEnd": p.period_end,
                "payPeriodType": p.pay_period_type,
                "totalHours": crate::jsfmt::num(p.total_hours),
                "totalAmount": fnum(p.total_amount),
                "currency": p.currency,
                "approverName": p.approver_name,
                "rejectedReason": p.rejected_reason,
            }),
            category: "Timesheet Status",
            label: "send timesheet status email",
        },
    );
}

pub struct InvoicePaid {
    pub employee_email: String,
    pub employee_name: String,
    pub invoice_number: String,
    pub company_name: String,
    pub period_start: String,
    pub period_end: String,
    pub total_amount: String,
    pub currency: String,
    pub paid_at: String,
    pub pdf: Vec<u8>,
}

/// Lampiran PDF lewat `mz_core::mail::deliver`. Node meng-`await`
/// pengiriman ini, jadi fungsi ini juga ditunggu. Galat ditelan (`false`).
pub async fn send_invoice_paid(state: &AppState, p: InvoicePaid) -> bool {
    let html = match render(
        "invoice-paid",
        &json!({
            "employeeName": p.employee_name,
            "invoiceNumber": p.invoice_number,
            "companyName": p.company_name,
            "periodStart": p.period_start,
            "periodEnd": p.period_end,
            "totalAmount": p.total_amount,
            "currency": p.currency,
            "paidAt": p.paid_at,
        }),
    ) {
        Ok(h) => h,
        Err(err) => {
            tracing::error!(error = %err, "invoice-paid template error");
            return false;
        }
    };
    let subject = format!("Your Invoice {} Has Been Paid", p.invoice_number);
    let filename = format!("{}.pdf", p.invoice_number);
    mail::deliver(
        state,
        "send invoice paid email",
        &Message {
            to: &[Recipient {
                email: &p.employee_email,
                name: Some(&p.employee_name),
            }],
            subject: &subject,
            text: &subject,
            html: &html,
            category: "Invoice Paid",
            attachments: &[Attachment {
                filename: &filename,
                mime: "application/pdf",
                content: &p.pdf,
            }],
            timeout: Message::DEFAULT_TIMEOUT,
        },
    )
    .await
    .success
}

/// Jenis email manager summary (`MailService.sendManagerSummaryDaily/Weekly`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryKind {
    Daily,
    Weekly,
}

impl SummaryKind {
    pub const fn template(self) -> &'static str {
        match self {
            Self::Daily => "manager-summary-daily",
            Self::Weekly => "manager-summary-weekly",
        }
    }
    pub const fn category(self) -> &'static str {
        match self {
            Self::Daily => "Manager Summary Daily",
            Self::Weekly => "Manager Summary Weekly",
        }
    }
}

/// Subjek email summary: `Daily Team Summary — <subjectDate>` /
/// `Weekly Team Summary — <start> – <end>` (em dash + en dash, persis Node).
pub fn summary_subject(kind: SummaryKind, ctx: &Value) -> String {
    let s = |k: &str| ctx.get(k).and_then(Value::as_str).unwrap_or("undefined");
    match kind {
        SummaryKind::Daily => format!("Daily Team Summary \u{2014} {}", s("subjectDate")),
        SummaryKind::Weekly => format!(
            "Weekly Team Summary \u{2014} {} \u{2013} {}",
            s("startDateFormatted"),
            s("endDateFormatted")
        ),
    }
}

/// Penerima summary: satu panggilan ke semua penerima (`to` = daftar email, nama = email).
fn summary_recipients(to: &[String]) -> Vec<Recipient<'_>> {
    to.iter()
        .map(|e| Recipient {
            email: e,
            name: Some(e),
        })
        .collect()
}

fn summary_message<'a>(
    kind: SummaryKind,
    to: &'a [Recipient<'a>],
    subject: &'a str,
    html: &'a str,
) -> Message<'a> {
    Message {
        to,
        subject,
        text: subject,
        html,
        category: kind.category(),
        attachments: &[],
        timeout: Message::DEFAULT_TIMEOUT,
    }
}

/// Body Mailtrap summary dengan HTML yang sudah dirender. Murni (tanpa jaringan) supaya bisa diuji.
pub fn summary_body_html(
    kind: SummaryKind,
    from_email: Option<&str>,
    to: &[String],
    ctx: &Value,
    html: &str,
) -> Value {
    let subject = summary_subject(kind, ctx);
    let to = summary_recipients(to);
    mail::message_body(from_email, &summary_message(kind, &to, &subject, html))
}

/// `sendManagerSummaryDaily/Weekly`: `true` bila Mailtrap menerima. Galat ditelan (log) seperti
/// Node (`{ success: false }`). Tanpa `MAILTRAP_TOKEN` tidak keluar ke jaringan.
pub async fn send_manager_summary(
    state: &AppState,
    kind: SummaryKind,
    to: &[String],
    ctx: &Value,
) -> bool {
    let html = match render(kind.template(), ctx) {
        Ok(h) => h,
        Err(err) => {
            tracing::error!(error = %err, template = kind.template(), "send manager summary failed");
            return false;
        }
    };
    let subject = summary_subject(kind, ctx);
    let to = summary_recipients(to);
    mail::deliver(
        state,
        "send manager summary",
        &summary_message(kind, &to, &subject, &html),
    )
    .await
    .success
}
