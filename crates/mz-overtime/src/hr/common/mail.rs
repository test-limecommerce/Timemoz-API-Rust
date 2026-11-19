//! Port `src/overtime/overtime-mail.ts` + tiga `MailService.sendOvertime*`
//! (`mail.service.ts:2629-2730`). Fire-and-forget: setiap email dikirim di latar lewat
//! `mz_core::bg::spawn`; kegagalan hanya di-`tracing::warn!` dan tidak pernah sampai ke
//! alur bisnis (pengajuan/keputusan sudah tersimpan). Fungsi kirim kembali seketika.
//!
//! Isi email dibangun fungsi murni `build_*` (diuji tanpa jaringan).

use mz_core::AppState;
use mz_core::mail::{self, OwnedMail};
use serde_json::{Value, json};

use super::enums::OvertimeRequestKind;
use crate::math::format_hours;

/// `OvertimeMailRecipient`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OvertimeMailRecipient {
    pub email: String,
    pub name: String,
}

/// Keputusan untuk [`decided`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OvertimeDecision {
    Approved,
    Rejected,
}

const MONTHS_ID: [&str; 12] = [
    "Januari",
    "Februari",
    "Maret",
    "April",
    "Mei",
    "Juni",
    "Juli",
    "Agustus",
    "September",
    "Oktober",
    "November",
    "Desember",
];

/// `formatLeaveDate(dateKey)`: `Intl.DateTimeFormat('id-ID', { dateStyle: 'long' })` →
/// `22 Oktober 2026`. Kunci tak valid dikembalikan apa adanya (Node: `Invalid Date` → throw,
/// lalu email gagal dan dicatat).
pub fn format_date_label(date_key: &str) -> String {
    use chrono::Datelike;
    match crate::zone::parse_date_key(date_key) {
        Some(d) => {
            let month = MONTHS_ID
                .get(d.month0() as usize)
                .copied()
                .unwrap_or_default();
            format!("{} {month} {}", d.day(), d.year())
        }
        None => date_key.to_owned(),
    }
}

/// `singleLine(value)`: CR/LF berturut → satu spasi, lalu trim.
pub fn single_line(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut in_break = false;
    for c in value.chars() {
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
    out.trim().to_owned()
}

fn host_fe(state: &AppState) -> Option<String> {
    state
        .config
        .var("HOST_FE")
        .map(|h| h.trim_end_matches('/').to_owned())
        .filter(|h| !h.is_empty())
}

fn owned(
    to: &OvertimeMailRecipient,
    subject: String,
    template: &'static str,
    context: Value,
    category: &'static str,
    label: &'static str,
) -> OwnedMail {
    OwnedMail {
        to_email: to.email.clone(),
        to_name: to.name.clone(),
        subject,
        text: None,
        template,
        context,
        category,
        label,
    }
}

/// Isi `sendOvertimeRequestSubmitted` per approver (yang tanpa email dilewati).
pub fn build_request_submitted(
    host_fe: Option<&str>,
    approvers: &[OvertimeMailRecipient],
    employee_name: &str,
    date_key: &str,
    requested_sec: i64,
    reason: &str,
) -> Vec<OwnedMail> {
    approvers
        .iter()
        .filter(|a| !a.email.is_empty())
        .map(|a| {
            owned(
                a,
                format!("Pengajuan lembur baru: {}", single_line(employee_name)),
                "overtime-request-submitted",
                json!({
                    "approverName": a.name,
                    "employeeName": employee_name,
                    "dateLabel": format_date_label(date_key),
                    "hoursLabel": format_hours(requested_sec),
                    "reason": reason,
                    "approvalUrl": host_fe.map(|h| format!("{h}/overtime-approvals")),
                }),
                "Overtime Request Submitted",
                "send Overtime Request Submitted email",
            )
        })
        .collect()
}

/// Isi `sendOvertimeOrderReceived` per karyawan (yang tanpa email dilewati).
pub fn build_order_received(
    host_fe: Option<&str>,
    employees: &[OvertimeMailRecipient],
    approver_name: &str,
    date_key: &str,
    requested_sec: i64,
    reason: &str,
) -> Vec<OwnedMail> {
    let date_label = format_date_label(date_key);
    employees
        .iter()
        .filter(|e| !e.email.is_empty())
        .map(|e| {
            owned(
                e,
                format!("Perintah lembur dari {}", single_line(approver_name)),
                "overtime-order-received",
                json!({
                    "employeeName": e.name,
                    "approverName": approver_name,
                    "dateLabel": date_label,
                    "hoursLabel": format_hours(requested_sec),
                    "reason": reason,
                    "deadlineLabel": format!("{date_label} pukul 23:59"),
                    "myOvertimeUrl": host_fe.map(|h| format!("{h}/overtime")),
                }),
                "Overtime Order Received",
                "send Overtime Order Received email",
            )
        })
        .collect()
}

/// Isi `sendOvertimeDecided`. `None` bila penerima tanpa email. `kind = ORDER`: penerima =
/// approver pemberi perintah, `decider_name` = karyawan.
#[allow(clippy::too_many_arguments)]
pub fn build_decided(
    to: &OvertimeMailRecipient,
    decision: OvertimeDecision,
    decider_name: &str,
    employee_name: &str,
    date_key: &str,
    requested_sec: i64,
    reject_reason: Option<&str>,
    kind: Option<OvertimeRequestKind>,
) -> Option<OwnedMail> {
    if to.email.is_empty() {
        return None;
    }
    let is_order = kind == Some(OvertimeRequestKind::ORDER);
    let is_approved = decision == OvertimeDecision::Approved;
    let decision_label = match (is_order, is_approved) {
        (true, true) => "Diterima",
        (_, false) => "Ditolak",
        (false, true) => "Disetujui",
    };
    let subject = match (is_order, is_approved) {
        (true, true) => "Perintah lembur Anda diterima",
        (true, false) => "Perintah lembur Anda ditolak",
        (false, true) => "Pengajuan lembur Anda disetujui",
        (false, false) => "Pengajuan lembur Anda ditolak",
    };
    let reject_reason = if is_approved {
        None
    } else {
        reject_reason.filter(|r| !r.is_empty())
    };
    Some(owned(
        to,
        subject.to_owned(),
        "overtime-decided",
        json!({
            "isOrder": is_order,
            "recipientName": to.name,
            "employeeName": employee_name,
            "deciderName": decider_name,
            "isApproved": is_approved,
            "decisionLabel": decision_label,
            "dateLabel": format_date_label(date_key),
            "hoursLabel": format_hours(requested_sec),
            "rejectReason": reject_reason,
        }),
        "Overtime Decided",
        "send Overtime Decided email",
    ))
}

/// `safeSend`: kirim di latar; gagal → `warn` "Email lembur <kind> ke <email> gagal".
fn safe_send(state: &AppState, kind: &'static str, m: OwnedMail) {
    let state = state.clone();
    mz_core::bg::spawn(async move {
        let res = mail::send(&state, m.as_mail()).await;
        if !res.success {
            tracing::warn!(
                "Email lembur {kind} ke {} gagal: {}",
                m.to_email,
                res.message
            );
        }
    });
}

/// `OvertimeMail.requestSubmitted`: satu email per approver ber-email.
pub fn request_submitted(
    state: &AppState,
    approvers: &[OvertimeMailRecipient],
    employee_name: &str,
    date_key: &str,
    requested_sec: i64,
    reason: &str,
) {
    let host = host_fe(state);
    for m in build_request_submitted(
        host.as_deref(),
        approvers,
        employee_name,
        date_key,
        requested_sec,
        reason,
    ) {
        safe_send(state, "request-submitted", m);
    }
}

/// `OvertimeMail.orderReceived`: satu email per karyawan ber-email.
pub fn order_received(
    state: &AppState,
    employees: &[OvertimeMailRecipient],
    approver_name: &str,
    date_key: &str,
    requested_sec: i64,
    reason: &str,
) {
    let host = host_fe(state);
    for m in build_order_received(
        host.as_deref(),
        employees,
        approver_name,
        date_key,
        requested_sec,
        reason,
    ) {
        safe_send(state, "order-received", m);
    }
}

/// `OvertimeMail.decided`: email keputusan ke pengaju (atau approver pemberi perintah).
#[allow(clippy::too_many_arguments)]
pub fn decided(
    state: &AppState,
    to: &OvertimeMailRecipient,
    decision: OvertimeDecision,
    decider_name: &str,
    employee_name: &str,
    date_key: &str,
    requested_sec: i64,
    reject_reason: Option<&str>,
    kind: Option<OvertimeRequestKind>,
) {
    if let Some(m) = build_decided(
        to,
        decision,
        decider_name,
        employee_name,
        date_key,
        requested_sec,
        reject_reason,
        kind,
    ) {
        safe_send(state, "decided", m);
    }
}

#[cfg(test)]
mod tests {
    //! Kasus `overtime-mail.spec.ts` (pada pembangun isi; pengiriman = Mailtrap).
    #![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn r(email: &str, name: &str) -> OvertimeMailRecipient {
        OvertimeMailRecipient {
            email: email.into(),
            name: name.into(),
        }
    }

    #[test]
    fn one_mail_per_approver_skipping_empty() {
        let mails = build_request_submitted(
            Some("http://fe.test"),
            &[r("a@x.com", "A"), r("", "B"), r("c@x.com", "C")],
            "Budi",
            "2026-10-22",
            7200,
            "Deadline",
        );
        assert_eq!(mails.len(), 2);
        let first = mails
            .first()
            .map(|m| (m.to_email.as_str(), m.to_name.as_str()));
        assert_eq!(first, Some(("a@x.com", "A")));
        let ctx = mails.first().map(|m| m.context.clone()).unwrap_or_default();
        assert_eq!(ctx["reason"], "Deadline");
        assert_eq!(ctx["dateLabel"], "22 Oktober 2026");
        assert_eq!(ctx["hoursLabel"], "2");
        assert_eq!(ctx["approvalUrl"], "http://fe.test/overtime-approvals");
        assert_eq!(
            mails.first().map(|m| m.subject.as_str()),
            Some("Pengajuan lembur baru: Budi")
        );
    }

    #[test]
    fn decided_forwards_reject_reason_and_decision() {
        let m = build_decided(
            &r("e@x.com", "Budi"),
            OvertimeDecision::Rejected,
            "Sari",
            "Budi",
            "2026-10-22",
            5400,
            Some("Tidak perlu"),
            None,
        );
        let m = m.map(|m| (m.to_email, m.subject, m.context));
        let (to, subject, ctx) = m.unwrap_or_default();
        assert_eq!(to, "e@x.com");
        assert_eq!(subject, "Pengajuan lembur Anda ditolak");
        assert_eq!(ctx["isApproved"], false);
        assert_eq!(ctx["rejectReason"], "Tidak perlu");
        assert_eq!(ctx["hoursLabel"], "1,5");
    }

    #[test]
    fn decided_approved_drops_reject_reason_and_skips_empty_email() {
        let m = build_decided(
            &r("e@x.com", "Budi"),
            OvertimeDecision::Approved,
            "Sari",
            "Budi",
            "2026-10-22",
            3600,
            Some("x"),
            None,
        );
        assert_eq!(
            m.map(|m| m.context["rejectReason"].clone()),
            Some(Value::Null)
        );
        assert!(
            build_decided(
                &r("", "Budi"),
                OvertimeDecision::Approved,
                "Sari",
                "Budi",
                "2026-10-22",
                3600,
                None,
                None
            )
            .is_none()
        );
    }

    #[test]
    fn order_received_per_employee() {
        let mails = build_order_received(
            None,
            &[r("a@x.com", "A"), r("", "B"), r("c@x.com", "C")],
            "Sari",
            "2026-10-24",
            7200,
            "Audit",
        );
        assert_eq!(mails.len(), 2);
        let ctx = mails.first().map(|m| m.context.clone()).unwrap_or_default();
        assert_eq!(ctx["employeeName"], "A");
        assert_eq!(ctx["approverName"], "Sari");
        assert_eq!(ctx["reason"], "Audit");
        assert_eq!(ctx["deadlineLabel"], "24 Oktober 2026 pukul 23:59");
        assert_eq!(ctx["myOvertimeUrl"], Value::Null);
        assert_eq!(
            mails.first().map(|m| m.subject.as_str()),
            Some("Perintah lembur dari Sari")
        );
    }

    #[test]
    fn decided_forwards_kind() {
        let m = build_decided(
            &r("a@x.com", "Sari"),
            OvertimeDecision::Approved,
            "Budi",
            "Budi",
            "2026-10-24",
            3600,
            None,
            Some(OvertimeRequestKind::ORDER),
        );
        let (subject, ctx) = m.map(|m| (m.subject, m.context)).unwrap_or_default();
        assert_eq!(subject, "Perintah lembur Anda diterima");
        assert_eq!(ctx["isOrder"], true);
        assert_eq!(ctx["decisionLabel"], "Diterima");
    }

    #[test]
    fn helpers() {
        assert_eq!(single_line(" a\r\n\nb \n"), "a b");
        assert_eq!(format_date_label("2026-01-05"), "5 Januari 2026");
    }
}
