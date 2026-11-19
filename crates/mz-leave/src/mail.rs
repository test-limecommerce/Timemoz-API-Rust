//! Email cuti (`mail.service.ts:2554-2626` + `leaveMailContext`, `buildLeaveIcs`,
//! `postLeaveMail`): `sendLeaveRequestToApprover` dan `sendLeaveDecisionToRequester`.
//! Tidak pernah melempar: kegagalan → `MailResult { success: false }` + log.

use chrono::{Duration, NaiveDateTime};
use mz_core::AppState;
use mz_core::mail::{self, Attachment, MailResult, Message, Recipient};
use mz_overtime::hr::common::mail::{format_date_label, single_line};
use serde_json::Value;

use crate::day_count::{parse_date_key, to_date_key};
use mz_int_gcal::mail::{fold_line, nl, param, text};

/// `LeaveMailDetails` (tanggal = kunci `YYYY-MM-DD`).
#[derive(Debug, Clone, PartialEq)]
pub struct LeaveMailDetails {
    pub employee_name: String,
    pub leave_type_name: String,
    pub start_date: String,
    pub end_date: String,
    pub start_day_type: String,
    pub end_day_type: String,
    pub total_days: f64,
    pub reason: String,
}

/// `leaveMailContext(payload)`. Catatan: `dayLabel` Node membandingkan dengan
/// `FIRST_HALF`/`SECOND_HALF` (bukan nilai enum `LeaveDayType`), jadi praktis selalu `null`.
pub fn leave_mail_context(p: &LeaveMailDetails) -> serde_json::Map<String, Value> {
    let day_label = |t: &str| match t {
        "FIRST_HALF" => Value::from("setengah hari pertama"),
        "SECOND_HALF" => Value::from("setengah hari kedua"),
        _ => Value::Null,
    };
    let mut m = serde_json::Map::new();
    m.insert("employeeName".into(), Value::from(p.employee_name.clone()));
    m.insert(
        "leaveTypeName".into(),
        Value::from(p.leave_type_name.clone()),
    );
    m.insert(
        "startDate".into(),
        Value::from(format_date_label(&p.start_date)),
    );
    m.insert(
        "endDate".into(),
        Value::from(format_date_label(&p.end_date)),
    );
    m.insert("startDayLabel".into(), day_label(&p.start_day_type));
    m.insert("endDayLabel".into(), day_label(&p.end_day_type));
    m.insert("totalDays".into(), mz_core::jsfmt::num(p.total_days));
    m.insert("reason".into(), Value::from(p.reason.clone()));
    m
}

/// `postLeaveMail` → `{ success, message: "send <category> email success" }`.
async fn post_leave_mail(
    state: &AppState,
    to_email: &str,
    to_name: &str,
    subject: &str,
    html: &str,
    category: &str,
    attachments: &[Attachment<'_>],
) -> MailResult {
    let label = format!("send {category} email");
    mail::deliver(
        state,
        &label,
        &Message {
            to: &[Recipient {
                email: to_email,
                name: Some(to_name),
            }],
            subject,
            text: subject,
            html,
            category,
            attachments,
            timeout: Message::DEFAULT_TIMEOUT,
        },
    )
    .await
}

fn failed(message: &str) -> MailResult {
    MailResult {
        success: false,
        message: message.to_owned(),
        data: None,
    }
}

/// Subjek `sendLeaveRequestToApprover`.
pub fn approver_subject(employee_name: &str, is_reminder: bool) -> String {
    if is_reminder {
        format!(
            "[Pengingat] Pengajuan cuti dari {}",
            single_line(employee_name)
        )
    } else {
        format!("Pengajuan cuti baru dari {}", single_line(employee_name))
    }
}

/// `sendLeaveRequestToApprover({ ...details, approverEmail, approverName, isReminder })`.
pub async fn send_leave_request_to_approver(
    state: &AppState,
    details: &LeaveMailDetails,
    approver_email: &str,
    approver_name: &str,
    is_reminder: bool,
) -> MailResult {
    let mut ctx = leave_mail_context(details);
    ctx.insert("approverName".into(), Value::from(approver_name));
    ctx.insert("isReminder".into(), Value::from(is_reminder));
    let html = match mail::render("leave-request-approver", &Value::Object(ctx)) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "mail template error");
            return failed("send leave request email failed");
        }
    };
    let res = post_leave_mail(
        state,
        approver_email,
        approver_name,
        &approver_subject(&details.employee_name, is_reminder),
        &html,
        "Leave Request Approver",
        &[],
    )
    .await;
    if res.success {
        res
    } else {
        failed("send leave request email failed")
    }
}

/// Keputusan untuk [`send_leave_decision_to_requester`].
#[derive(Debug, Clone, PartialEq)]
pub enum LeaveDecision {
    Approved { approval_note: Option<String> },
    Rejected { rejection_reason: Option<String> },
}

/// `sendLeaveDecisionToRequester({...})`: disetujui → lampiran `cuti.ics`; ditolak → alasan
/// di badan email.
pub async fn send_leave_decision_to_requester(
    state: &AppState,
    details: &LeaveMailDetails,
    employee_email: &str,
    reviewer_name: &str,
    leave_uid: &str,
    decision: &LeaveDecision,
) -> MailResult {
    let is_approved = matches!(decision, LeaveDecision::Approved { .. });
    let mut ctx = leave_mail_context(details);
    ctx.insert("reviewerName".into(), Value::from(reviewer_name));
    ctx.insert("isApproved".into(), Value::from(is_approved));
    let (note, reason) = match decision {
        LeaveDecision::Approved { approval_note } => (approval_note.clone(), None),
        LeaveDecision::Rejected { rejection_reason } => (None, rejection_reason.clone()),
    };
    ctx.insert("approvalNote".into(), note.map_or(Value::Null, Value::from));
    ctx.insert(
        "rejectionReason".into(),
        reason.map_or(Value::Null, Value::from),
    );
    let html = match mail::render("leave-request-status", &Value::Object(ctx)) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "mail template error");
            return failed("send leave decision email failed");
        }
    };
    let ics = if is_approved {
        match build_leave_ics(
            details,
            employee_email,
            leave_uid,
            mz_core::time::now_naive(),
        ) {
            Some(v) => Some(v),
            None => return failed("send leave decision email failed"),
        }
    } else {
        None
    };
    let attachments: Vec<Attachment<'_>> = ics
        .as_deref()
        .map(|v| Attachment {
            filename: "cuti.ics",
            mime: "text/calendar",
            content: v.as_bytes(),
        })
        .into_iter()
        .collect();
    let subject = if is_approved {
        "Pengajuan cuti Anda disetujui"
    } else {
        "Pengajuan cuti Anda ditolak"
    };
    let res = post_leave_mail(
        state,
        employee_email,
        &details.employee_name,
        subject,
        &html,
        "Leave Request Status",
        &attachments,
    )
    .await;
    if res.success {
        res
    } else {
        failed("send leave decision email failed")
    }
}

// ── ICS (paket `ics`, sama dengan `mz_int_gcal::mail::build_ics`) ────────────

/// `buildLeaveIcs(payload)`: event seharian `[start, end + 1)`, `OOF`, satu peserta.
/// `None` = tanggal tidak valid (Node: `createEvent` mengembalikan `error`).
pub fn build_leave_ics(
    p: &LeaveMailDetails,
    employee_email: &str,
    leave_uid: &str,
    now: NaiveDateTime,
) -> Option<String> {
    let start = parse_date_key(&p.start_date)?;
    let exclusive_end = parse_date_key(&p.end_date)? + Duration::days(1);
    let compact = |d: chrono::NaiveDate| to_date_key(d).replace('-', "");
    let mut s = String::new();
    s.push_str("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nCALSCALE:GREGORIAN\r\n");
    s.push_str("PRODID:adamgibbons/ics\r\nMETHOD:PUBLISH\r\nX-PUBLISHED-TTL:PT1H\r\n");
    s.push_str("BEGIN:VEVENT\r\n");
    let mut line = |l: String| {
        s.push_str(&fold_line(&l));
        s.push_str("\r\n");
    };
    line(format!(
        "UID:{}",
        nl(&format!("leave-{leave_uid}@timemoz.com"))
    ));
    let title = format!("Cuti: {}", single_line(&p.leave_type_name));
    line(format!("SUMMARY:{}", nl(&text(&title))));
    line(format!("DTSTAMP:{}", now.format("%Y%m%dT%H%M%SZ")));
    line(format!("DTSTART;VALUE=DATE:{}", compact(start)));
    if start != exclusive_end {
        line(format!("DTEND;VALUE=DATE:{}", compact(exclusive_end)));
    }
    if !p.reason.is_empty() {
        line(format!("DESCRIPTION:{}", nl(&text(&p.reason))));
    }
    line("STATUS:CONFIRMED".into());
    line("X-MICROSOFT-CDO-BUSYSTATUS:OOF".into());
    let name = if p.employee_name.is_empty() {
        "Unnamed attendee"
    } else {
        &p.employee_name
    };
    let mut att = format!("CN={}", param(name));
    if !employee_email.is_empty() {
        att.push_str(&format!(":mailto:{employee_email}"));
    }
    line(format!("ATTENDEE;{}", nl(&att)));
    s.push_str("END:VEVENT\r\nEND:VCALENDAR\r\n");
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn details() -> LeaveMailDetails {
        LeaveMailDetails {
            employee_name: "Budi \"B\"".into(),
            leave_type_name: "Tahunan, khusus; x".into(),
            start_date: "2026-03-05".into(),
            end_date: "2026-03-06".into(),
            start_day_type: "FULL_DAY".into(),
            end_day_type: "HALF_DAY_MORNING".into(),
            total_days: 1.5,
            reason: "Liburan\nkeluarga, ya; ok".into(),
        }
    }

    // Keluaran acuan: `createEvent` paket `ics` di Mudah-API (dijalankan 2026-11-17).
    #[test]
    fn ics_matches_node_package() {
        let now = mz_core::time::parse_js_date("2026-11-17T06:42:35Z").unwrap_or_default();
        let got = build_leave_ics(&details(), "b@x.com", "abc", now).unwrap_or_default();
        assert_eq!(
            got,
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nCALSCALE:GREGORIAN\r\nPRODID:adamgibbons/ics\r\nMETHOD:PUBLISH\r\nX-PUBLISHED-TTL:PT1H\r\nBEGIN:VEVENT\r\nUID:leave-abc@timemoz.com\r\nSUMMARY:Cuti: Tahunan\\, khusus\\; x\r\nDTSTAMP:20261117T064235Z\r\nDTSTART;VALUE=DATE:20260305\r\nDTEND;VALUE=DATE:20260307\r\nDESCRIPTION:Liburan\\nkeluarga\\, ya\\; ok\r\nSTATUS:CONFIRMED\r\nX-MICROSOFT-CDO-BUSYSTATUS:OOF\r\nATTENDEE;CN=\"Budi \\\"B\\\"\":mailto:b@x.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        let empty = LeaveMailDetails {
            employee_name: String::new(),
            leave_type_name: "X".into(),
            reason: String::new(),
            end_date: "2026-03-05".into(),
            ..details()
        };
        let got = build_leave_ics(&empty, "", "abc", now).unwrap_or_default();
        assert!(got.contains("DTEND;VALUE=DATE:20260306\r\nSTATUS:CONFIRMED"));
        assert!(got.contains("ATTENDEE;CN=\"Unnamed attendee\"\r\nEND:VEVENT"));
        assert!(
            build_leave_ics(
                &LeaveMailDetails {
                    start_date: "x".into(),
                    ..details()
                },
                "",
                "a",
                now
            )
            .is_none()
        );
    }

    #[test]
    fn context_and_subject() {
        let ctx = leave_mail_context(&details());
        assert_eq!(ctx.get("startDate"), Some(&Value::from("5 Maret 2026")));
        assert_eq!(ctx.get("startDayLabel"), Some(&Value::Null));
        assert_eq!(ctx.get("totalDays"), Some(&json!(1.5)));
        assert_eq!(
            approver_subject("Budi\r\nX ", true),
            "[Pengingat] Pengajuan cuti dari Budi X"
        );
        assert_eq!(
            approver_subject("Budi", false),
            "Pengajuan cuti baru dari Budi"
        );
    }

    #[test]
    fn templates_exist() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/mail/");
        for (name, var) in [
            ("leave-request-approver", "isReminder"),
            ("leave-request-status", "rejectionReason"),
        ] {
            let src = std::fs::read_to_string(format!("{dir}{name}.hbs")).unwrap_or_default();
            assert!(src.contains(var), "{name}");
        }
    }
}
