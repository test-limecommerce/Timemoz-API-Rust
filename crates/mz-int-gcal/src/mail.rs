//! `MailService.sendCalendarEventInvite` + `buildIcsInvite` (paket `ics` 3.12.0).
//!
//! Pengiriman lewat `mz_core::mail::deliver` (lampiran `invite.ics`).

use chrono::{Datelike, NaiveDateTime, Timelike};
use mz_core::AppState;
use mz_core::mail::{self, Attachment, MailResult, Message, Recipient};
use serde_json::json;

const LABEL: &str = "send calendar event invite";

/// Argumen `sendCalendarEventInvite`.
#[derive(Debug, Clone)]
pub struct Invite {
    pub to: String,
    pub attendee_name: String,
    pub organizer_name: String,
    pub organizer_email: String,
    pub company_name: String,
    pub uid: String,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_at: NaiveDateTime,
    pub end_at: NaiveDateTime,
    pub is_all_day: bool,
}

/// `foldLine` (75 code point lalu 74, disambung `\r\n\t`).
pub fn fold_line(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut parts = Vec::new();
    let mut rest: &[char] = &chars;
    let mut len = 75;
    while rest.len() > len {
        let (head, tail) = rest.split_at(len);
        parts.push(head.iter().collect::<String>());
        rest = tail;
        len = 74;
    }
    parts.push(rest.iter().collect::<String>());
    parts.join("\r\n\t")
}

/// `encodeNewLines`.
pub fn nl(s: &str) -> String {
    s.replace("\r\n", "\\n").replace('\n', "\\n")
}

/// `formatText`.
pub fn text(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace("\r\n", "\\n")
        .replace('\n', "\\n")
        .replace(';', "\\;")
        .replace(',', "\\,")
}

/// `encodeParamValue`.
pub fn param(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\\\""))
}

fn utc_stamp(t: NaiveDateTime) -> String {
    t.format("%Y%m%dT%H%M%SZ").to_string()
}

/// `createEvent({...})` → teks `.ics` (DTSTAMP = `now`).
pub fn build_ics(p: &Invite, now: NaiveDateTime) -> String {
    let mut s = String::new();
    s.push_str("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nCALSCALE:GREGORIAN\r\n");
    s.push_str("PRODID:adamgibbons/ics\r\nMETHOD:PUBLISH\r\nX-PUBLISHED-TTL:PT1H\r\n");
    s.push_str("BEGIN:VEVENT\r\n");
    let mut line = |l: String| {
        s.push_str(&fold_line(&l));
        s.push_str("\r\n");
    };
    line(format!("UID:{}", nl(&format!("{}@timemoz.com", p.uid))));
    if !p.title.is_empty() {
        line(format!("SUMMARY:{}", nl(&text(&p.title))));
    }
    line(format!("DTSTAMP:{}", utc_stamp(now)));
    let date = |t: NaiveDateTime| format!("{}{:02}{:02}", t.year(), t.month(), t.day());
    // Array [y, m, d, H, M] input/output UTC → detik selalu 00.
    let dt = |t: NaiveDateTime| format!("{}T{:02}{:02}00Z", date(t), t.hour(), t.minute());
    if p.is_all_day {
        line(format!("DTSTART;VALUE=DATE:{}", date(p.start_at)));
        if date(p.start_at) != date(p.end_at) {
            line(format!("DTEND;VALUE=DATE:{}", date(p.end_at)));
        }
    } else {
        line(format!("DTSTART:{}", dt(p.start_at)));
        line(format!("DTEND:{}", dt(p.end_at)));
    }
    if let Some(d) = p.description.as_deref().filter(|d| !d.is_empty()) {
        line(format!("DESCRIPTION:{}", nl(&text(d))));
    }
    if let Some(l) = p.location.as_deref().filter(|l| !l.is_empty()) {
        line(format!("LOCATION:{}", nl(&text(l))));
    }
    line("STATUS:CONFIRMED".into());
    let org_name = if p.organizer_name.is_empty() {
        "Organizer"
    } else {
        &p.organizer_name
    };
    let mut org = format!("ORGANIZER;CN={}", param(org_name));
    if !p.organizer_email.is_empty() {
        org.push_str(&format!(":MAILTO:{}", p.organizer_email));
    }
    line(org);
    let att_name = if p.attendee_name.is_empty() {
        "Unnamed attendee"
    } else {
        &p.attendee_name
    };
    let mut att = format!("RSVP=TRUE;CN={}", param(att_name));
    if !p.to.is_empty() {
        att.push_str(&format!(":mailto:{}", p.to));
    }
    line(format!("ATTENDEE;{}", nl(&att)));
    s.push_str("END:VEVENT\r\nEND:VCALENDAR\r\n");
    s
}

/// `formatDate` email: `2026-05-20 02:00 UTC`.
fn human(t: NaiveDateTime) -> String {
    format!("{} UTC", t.format("%Y-%m-%d %H:%M"))
}

fn fail() -> MailResult {
    MailResult {
        success: false,
        message: format!("{LABEL} failed"),
        data: None,
    }
}

/// `sendCalendarEventInvite(payload)` → `{ success, message }` (tidak pernah melempar).
pub async fn send_invite(state: &AppState, p: &Invite) -> MailResult {
    let ics = build_ics(p, mz_core::time::now_naive());
    let html = match mail::render(
        "calendar-event-invite",
        &json!({
            "attendeeName": p.attendee_name,
            "organizerName": p.organizer_name,
            "companyName": p.company_name,
            "title": p.title,
            "description": p.description,
            "location": p.location,
            "startAt": human(p.start_at),
            "endAt": human(p.end_at),
        }),
    ) {
        Ok(h) => h,
        Err(err) => {
            tracing::error!(error = %err, "mail template error");
            return fail();
        }
    };
    let subject = format!("Invitation: {}", p.title);
    let text = format!("{} invited you to \"{}\"", p.organizer_name, p.title);
    mail::deliver(
        state,
        LABEL,
        &Message {
            to: &[Recipient {
                email: &p.to,
                name: Some(&p.attendee_name),
            }],
            subject: &subject,
            text: &text,
            html: &html,
            category: "Calendar Event Invite",
            attachments: &[Attachment {
                filename: "invite.ics",
                mime: "text/calendar",
                content: ics.as_bytes(),
            }],
            timeout: Message::DEFAULT_TIMEOUT,
        },
    )
    .await
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        crate::util::parse_js_date(s).unwrap()
    }

    #[test]
    fn ics_shape() {
        let p = Invite {
            to: "budi@co.com".into(),
            attendee_name: "Budi".into(),
            organizer_name: "Yudha".into(),
            organizer_email: String::new(),
            company_name: "Acme".into(),
            uid: "u-1".into(),
            title: "Sprint, planning".into(),
            description: Some("a\nb".into()),
            location: None,
            start_at: at("2026-05-20T09:00:00+07:00"),
            end_at: at("2026-05-20T10:00:00+07:00"),
            is_all_day: false,
        };
        let ics = build_ics(&p, at("2026-05-01T00:00:00Z"));
        assert_eq!(
            ics,
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nCALSCALE:GREGORIAN\r\nPRODID:adamgibbons/ics\r\n\
             METHOD:PUBLISH\r\nX-PUBLISHED-TTL:PT1H\r\nBEGIN:VEVENT\r\nUID:u-1@timemoz.com\r\n\
             SUMMARY:Sprint\\, planning\r\nDTSTAMP:20260501T000000Z\r\nDTSTART:20260520T020000Z\r\n\
             DTEND:20260520T030000Z\r\nDESCRIPTION:a\\nb\r\nSTATUS:CONFIRMED\r\n\
             ORGANIZER;CN=\"Yudha\"\r\nATTENDEE;RSVP=TRUE;CN=\"Budi\":mailto:budi@co.com\r\n\
             END:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        let long = fold_line(&"x".repeat(160));
        assert_eq!(
            long.split("\r\n\t").map(str::len).collect::<Vec<_>>(),
            vec![75, 74, 11]
        );
        assert_eq!(human(p.start_at), "2026-05-20 02:00 UTC");
    }
}
