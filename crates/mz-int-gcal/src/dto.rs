//! DTO `src/integrations/google-calendar/dto/*` + validasi gaya class-validator
//! (`ValidationPipe({ transform: true })`). Urutan pesan per properti = urutan dekorator
//! dibalik (metadata class-validator didaftarkan dari bawah ke atas).

use mz_core::ApiError;
use mz_core::extract::Validate;
use mz_core::iso8601::is_iso8601;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::util::{QVal, js_number, js_number_str, parse_query, query_get};

/// `@Type(() => Number)` pada nilai query.
fn qnum(v: Option<&QVal>) -> Option<f64> {
    match v? {
        QVal::One(s) => Some(js_number_str(s)),
        QVal::Many(_) | QVal::Obj => Some(f64::NAN),
    }
}

fn is_int(n: f64) -> bool {
    n.is_finite() && n.fract() == 0.0
}

/// `CalendarMonthDto` (`?year&month`).
#[derive(Debug, Clone, Copy)]
pub struct CalendarMonth {
    pub year: f64,
    pub month: f64,
}

impl CalendarMonth {
    pub fn from_query(raw: Option<&str>) -> Result<Self, ApiError> {
        let q = parse_query(raw);
        let mut errs = Vec::new();
        let mut field = |name: &str, min: f64, max: f64| -> f64 {
            // `undefined` tetap undefined (bukan NaN), tapi semua validator gagal sama saja.
            let n = qnum(query_get(&q, name)).unwrap_or(f64::NAN);
            if n.is_nan() || n > max {
                errs.push(format!("{name} must not be greater than {max}"));
            }
            if n.is_nan() || n < min {
                errs.push(format!("{name} must not be less than {min}"));
            }
            if !is_int(n) {
                errs.push(format!("{name} must be an integer number"));
            }
            n
        };
        let year = field("year", 2000.0, 2100.0);
        let month = field("month", 1.0, 12.0);
        if errs.is_empty() {
            Ok(Self { year, month })
        } else {
            Err(ApiError::Validation(errs))
        }
    }
}

/// `CalendarQueryDto`.
#[derive(Debug, Clone)]
pub struct CalendarQuery {
    pub start_date: String,
    pub end_date: String,
    pub calendar_id: Option<String>,
    pub source: Option<String>,
    pub skip: Option<f64>,
    pub take: Option<f64>,
}

const ISO_MSG: &str = "must be a valid ISO 8601 date string";

impl CalendarQuery {
    pub fn from_query(raw: Option<&str>) -> Result<Self, ApiError> {
        let q = parse_query(raw);
        let mut errs = Vec::new();
        let date = |name: &str, errs: &mut Vec<String>| -> String {
            match query_get(&q, name).and_then(QVal::as_str) {
                Some(s) if is_iso8601(s) => s.to_owned(),
                _ => {
                    errs.push(format!("{name} {ISO_MSG}"));
                    String::new()
                }
            }
        };
        let start_date = date("startDate", &mut errs);
        let end_date = date("endDate", &mut errs);
        let calendar_id = match query_get(&q, "calendarId") {
            None => None,
            Some(QVal::One(s)) => Some(s.clone()),
            Some(QVal::Many(_) | QVal::Obj) => {
                errs.push("calendarId must be a string".into());
                None
            }
        };
        let source = match query_get(&q, "source") {
            None => None,
            Some(v) => {
                let s = v
                    .as_str()
                    .filter(|s| ["google", "internal", "all"].contains(s));
                if s.is_none() {
                    errs.push(
                        "source must be one of the following values: google, internal, all".into(),
                    );
                }
                s.map(str::to_owned)
            }
        };
        let skip = qnum(query_get(&q, "skip"));
        if let Some(n) = skip {
            if n.is_nan() || n < 0.0 {
                errs.push("skip must not be less than 0".into());
            }
            if !n.is_finite() {
                errs.push("skip must be a number conforming to the specified constraints".into());
            }
        }
        let take = qnum(query_get(&q, "take"));
        if let Some(n) = take {
            if n.is_nan() || n > 100.0 {
                errs.push("take must not be greater than 100".into());
            }
            if n.is_nan() || n < 1.0 {
                errs.push("take must not be less than 1".into());
            }
            if !n.is_finite() {
                errs.push("take must be a number conforming to the specified constraints".into());
            }
        }
        if errs.is_empty() {
            Ok(Self {
                start_date,
                end_date,
                calendar_id,
                source,
                skip,
                take,
            })
        } else {
            Err(ApiError::Validation(errs))
        }
    }
}

/// Body mentah `POST .../events`; divalidasi manual supaya pesannya sama.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct EventBody(pub Map<String, Value>);

/// `CreateCompanyEventDto` / `CreatePersonalEventDto` setelah lolos validasi.
#[derive(Debug, Clone)]
pub struct CreateEvent {
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_at: String,
    pub end_at: String,
    pub is_all_day: Option<bool>,
    /// Sesudah `@Type(() => Number)`.
    pub attendee_user_ids: Option<Vec<f64>>,
}

/// `undefined`/`null` dilewati `@IsOptional()`.
fn present(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !v.is_null())
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn validate_event(m: &Map<String, Value>, with_attendees: bool) -> Vec<String> {
    let mut out = Vec::new();
    // title: IsString, IsNotEmpty, MaxLength(500).
    let title = m.get("title");
    let title_str = title.and_then(Value::as_str);
    if title_str.is_none_or(|s| utf16_len(s) > 500) {
        out.push("title must be shorter than or equal to 500 characters".into());
    }
    let empty = match title {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        _ => false,
    };
    if empty {
        out.push("title should not be empty".into());
    }
    if title_str.is_none() {
        out.push("title must be a string".into());
    }
    for (name, max) in [("description", 5000), ("location", 500)] {
        if let Some(v) = present(m.get(name)) {
            if v.as_str().is_none_or(|s| utf16_len(s) > max) {
                out.push(format!(
                    "{name} must be shorter than or equal to {max} characters"
                ));
            }
            if !v.is_string() {
                out.push(format!("{name} must be a string"));
            }
        }
    }
    for name in ["startAt", "endAt"] {
        if !m.get(name).and_then(Value::as_str).is_some_and(is_iso8601) {
            out.push(format!("{name} {ISO_MSG}"));
        }
    }
    if let Some(v) = present(m.get("isAllDay"))
        && !v.is_boolean()
    {
        out.push("isAllDay must be a boolean value".into());
    }
    if with_attendees && let Some(v) = present(m.get("attendeeUserIds")) {
        let nums = attendee_numbers(v);
        if nums.iter().any(|n| n.is_nan() || *n <= 0.0) {
            out.push("each value in attendeeUserIds must be a positive number".into());
        }
        if nums.iter().any(|n| !is_int(*n)) {
            out.push("each value in attendeeUserIds must be an integer number".into());
        }
        match v.as_array() {
            Some(a) if a.len() <= 50 => {}
            _ => out.push("attendeeUserIds must contain no more than 50 elements".into()),
        }
        if !v.is_array() {
            out.push("attendeeUserIds must be an array".into());
        }
    }
    out
}

/// `@Type(() => Number)` pada array (per elemen) atau nilai tunggal.
fn attendee_numbers(v: &Value) -> Vec<f64> {
    match v {
        Value::Array(a) => a.iter().map(js_number).collect(),
        other => vec![js_number(other)],
    }
}

impl EventBody {
    pub fn into_dto(self, with_attendees: bool) -> Result<CreateEvent, ApiError> {
        let errs = validate_event(&self.0, with_attendees);
        if !errs.is_empty() {
            return Err(ApiError::Validation(errs));
        }
        let m = self.0;
        let s = |k: &str| present(m.get(k)).and_then(Value::as_str).map(str::to_owned);
        Ok(CreateEvent {
            title: s("title").unwrap_or_default(),
            description: s("description"),
            location: s("location"),
            start_at: s("startAt").unwrap_or_default(),
            end_at: s("endAt").unwrap_or_default(),
            is_all_day: present(m.get("isAllDay")).and_then(Value::as_bool),
            attendee_user_ids: if with_attendees {
                present(m.get("attendeeUserIds")).map(attendee_numbers)
            } else {
                None
            },
        })
    }
}

/// `ValidatedJson` hanya dipakai untuk parse body (validasi di [`EventBody::into_dto`]).
impl Validate for EventBody {}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    fn errs(r: Result<impl std::fmt::Debug, ApiError>) -> Vec<String> {
        match r {
            Err(ApiError::Validation(v)) => v,
            other => panic!("harus validasi gagal: {other:?}"),
        }
    }

    #[test]
    fn month_messages() {
        assert_eq!(
            errs(CalendarMonth::from_query(Some("month=5"))),
            vec![
                "year must not be greater than 2100",
                "year must not be less than 2000",
                "year must be an integer number"
            ]
        );
        assert_eq!(
            errs(CalendarMonth::from_query(Some("year=2026&month=5.5"))),
            vec!["month must be an integer number"]
        );
        let ok = CalendarMonth::from_query(Some("year=2026&month=5")).unwrap();
        assert_eq!((ok.year, ok.month), (2026.0, 5.0));
    }

    #[test]
    fn query_messages() {
        assert_eq!(
            errs(CalendarQuery::from_query(Some(
                "startDate=x&take=0&source=foo"
            ))),
            vec![
                "startDate must be a valid ISO 8601 date string",
                "endDate must be a valid ISO 8601 date string",
                "source must be one of the following values: google, internal, all",
                "take must not be less than 1"
            ]
        );
        let q = CalendarQuery::from_query(Some(
            "startDate=2026-05-01T00:00:00Z&endDate=2026-05-31&skip=2",
        ))
        .unwrap();
        assert_eq!(q.skip, Some(2.0));
        assert!(q.take.is_none());
    }

    #[test]
    fn body_messages() {
        let b = EventBody(
            json!({"startAt": "2026-05-20", "attendeeUserIds": ["2", -1]})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert_eq!(
            errs(b.into_dto(true)),
            vec![
                "title must be shorter than or equal to 500 characters",
                "title should not be empty",
                "title must be a string",
                "endAt must be a valid ISO 8601 date string",
                "each value in attendeeUserIds must be a positive number"
            ]
        );
        let b = EventBody(
            json!({"title": "T", "startAt": "2026-05-20", "endAt": "2026-05-21", "attendeeUserIds": ["2"]})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert_eq!(b.into_dto(true).unwrap().attendee_user_ids, Some(vec![2.0]));
    }
}
