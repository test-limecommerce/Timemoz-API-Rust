//! Tulisan DB bersama: `company.update` / `users.update` (model ber-audit) dan
//! `googleCalendarEvent.upsert` (tidak ber-audit).

use chrono::NaiveDateTime;
use mz_audit::{AuditHandle, Lookup, Model};
use mz_core::AppState;
use mz_core::time::{JsDate, now_naive};
use serde_json::Value;
use sqlx::types::Json;
use sqlx::{PgExecutor, Postgres, QueryBuilder};

use crate::google::{Fail, GResult};
use crate::util::{js_template, parse_js_date, truthy};

/// Satu kolom di `data` Prisma `update`.
pub enum Set {
    Text(&'static str, Option<String>),
    Ts(&'static str, Option<NaiveDateTime>),
}

/// Pesan P2025 Prisma untuk `update` tanpa baris.
const RECORD_NOT_FOUND: &str = "An operation failed because it depends on one or more records that were required but not found. Record to update not found.";

/// `prisma.<model>.update({ where: { id }, data })` tanpa `select` + audit (pra-baca → tulis
/// → `record_update`). `updatedAt` ditambahkan pemanggil seperti Node.
pub async fn update_audited(
    state: &AppState,
    audit: &AuditHandle,
    model: Model,
    id: i32,
    sets: Vec<Set>,
) -> GResult<()> {
    let table = match model {
        Model::Users => "users",
        _ => "company",
    };
    let pre = audit.pre_read(model, Lookup::Id(id)).await;
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!("UPDATE {table} SET "));
    for (i, s) in sets.into_iter().enumerate() {
        if i > 0 {
            qb.push(", ");
        }
        match s {
            Set::Text(col, v) => {
                qb.push(format!("\"{col}\" = ")).push_bind(v);
            }
            Set::Ts(col, v) => {
                qb.push(format!("\"{col}\" = ")).push_bind(v);
            }
        }
    }
    qb.push(" WHERE id = ").push_bind(id);
    let n = qb.build().execute(&state.db).await?.rows_affected();
    if n == 0 {
        return Err(Fail::Msg(RECORD_NOT_FOUND.into()));
    }
    if let Some(row) = audit.result_row(&state.db, model, Lookup::Id(id)).await {
        audit.record_update(model, pre, &row).await;
    }
    Ok(())
}

/// `new Date(Date.now() + expires_in * 1000)`; `expires_in` tak valid → `Invalid Date`
/// (Prisma menolaknya).
pub fn expiry_from(expires_in: Option<&Value>) -> GResult<NaiveDateTime> {
    let secs = match expires_in {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    #[allow(clippy::cast_possible_truncation)]
    secs.filter(|s| s.is_finite())
        .map(|s| now_naive() + chrono::Duration::milliseconds((s * 1000.0) as i64))
        .ok_or_else(|| Fail::Msg(invalid_date("googleCalendarTokenExpiresAt")))
}

pub fn invalid_date(field: &str) -> String {
    format!("Invalid value for argument `{field}`: Provided Date object is invalid. Expected Date.")
}

/// String opsional dari respons Google (`x ?? null`; non-string → `String(x)`).
pub fn opt_str(v: Option<&Value>) -> Option<String> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(crate::util::js_string(other)),
    }
}

/// Satu baris upsert hasil sinkronisasi (`upsertEvents` / `upsertUserEvent`).
pub struct EventUpsert {
    pub google_event_id: String,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_at: NaiveDateTime,
    pub end_at: NaiveDateTime,
    pub is_all_day: bool,
    pub status: String,
    pub html_link: Option<String>,
    pub raw: Value,
    pub cancelled: bool,
}

fn event_date(v: Option<&Value>) -> Result<Option<NaiveDateTime>, ()> {
    let dt = v.and_then(|s| s.get("dateTime"));
    let d = v.and_then(|s| s.get("date"));
    let pick = if truthy(dt) {
        dt
    } else if truthy(d) {
        d
    } else {
        return Ok(None);
    };
    // `new Date(x)` — string tak valid = `Invalid Date` (objek truthy) → Prisma melempar.
    match pick.and_then(Value::as_str).and_then(parse_js_date) {
        Some(t) => Ok(Some(t)),
        None => Err(()),
    }
}

/// Bentuk `item` Google → baris; `Ok(None)` = dilewati (`continue`), `Err` = tanggal tak
/// valid (Prisma melempar saat menulis).
pub fn event_from_item(item: &Value, user_id: Option<i32>) -> GResult<Option<EventUpsert>> {
    if !truthy(item.get("id")) {
        return Ok(None);
    }
    let start = item.get("start");
    let is_all_day = !truthy(start.and_then(|s| s.get("dateTime")));
    let bad = || Fail::Msg(invalid_date("startAt"));
    let start_at = event_date(start).map_err(|()| bad())?;
    let end_at = event_date(item.get("end")).map_err(|()| bad())?;
    let (Some(start_at), Some(end_at)) = (start_at, end_at) else {
        return Ok(None);
    };
    let id = js_template(item.get("id"));
    let google_event_id = match user_id {
        Some(u) => format!("{id}:u{u}"),
        None => id,
    };
    let status = opt_str(item.get("status")).unwrap_or_else(|| "confirmed".into());
    Ok(Some(EventUpsert {
        google_event_id,
        title: opt_str(item.get("summary")).unwrap_or_else(|| "(no title)".into()),
        description: opt_str(item.get("description")),
        location: opt_str(item.get("location")),
        start_at,
        end_at,
        is_all_day,
        cancelled: item.get("status").and_then(Value::as_str) == Some("cancelled"),
        status,
        html_link: opt_str(item.get("htmlLink")),
        raw: item.clone(),
    }))
}

/// `googleCalendarEvent.upsert` kunci `(googleEventId, companyId, calendarId)`.
pub async fn upsert_synced<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
    user_id: Option<i32>,
    calendar_id: &str,
    e: &EventUpsert,
) -> sqlx::Result<()> {
    let now = now_naive();
    sqlx::query(
        r#"INSERT INTO "googleCalendarEvent"
             ("companyId", "userId", "calendarId", "googleEventId", title, description, location,
              "startAt", "endAt", "isAllDay", status, "htmlLink", "rawMeta", "syncedAt")
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
           ON CONFLICT ("googleEventId", "companyId", "calendarId") DO UPDATE SET
             title = EXCLUDED.title, description = EXCLUDED.description,
             location = EXCLUDED.location, "startAt" = EXCLUDED."startAt",
             "endAt" = EXCLUDED."endAt", "isAllDay" = EXCLUDED."isAllDay",
             status = EXCLUDED.status, "htmlLink" = EXCLUDED."htmlLink",
             "rawMeta" = EXCLUDED."rawMeta", "syncedAt" = EXCLUDED."syncedAt",
             "updatedAt" = EXCLUDED."syncedAt""#,
    )
    .bind(company_id)
    .bind(user_id)
    .bind(calendar_id)
    .bind(&e.google_event_id)
    .bind(&e.title)
    .bind(&e.description)
    .bind(&e.location)
    .bind(e.start_at)
    .bind(e.end_at)
    .bind(e.is_all_day)
    .bind(&e.status)
    .bind(&e.html_link)
    .bind(Json(&e.raw))
    .bind(now)
    .execute(exec)
    .await
    .map(|_| ())
}

/// Data event lokal/Google buatan `createCompanyEvent` / `createUserEvent`.
pub struct NewEvent {
    pub company_id: i32,
    pub user_id: Option<i32>,
    pub created_by: i32,
    pub calendar_id: String,
    pub google_event_id: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_at: NaiveDateTime,
    pub end_at: NaiveDateTime,
    pub is_all_day: bool,
    pub html_link: Option<String>,
    pub raw: Option<Value>,
    pub source: &'static str,
}

/// `select` `createCompanyEvent`/`createUserEvent`.
#[derive(sqlx::FromRow)]
pub struct SavedEvent {
    pub uuid: String,
    pub source: String,
    #[sqlx(rename = "googleEventId")]
    pub google_event_id: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    #[sqlx(rename = "startAt")]
    pub start_at: JsDate,
    #[sqlx(rename = "endAt")]
    pub end_at: JsDate,
    #[sqlx(rename = "isAllDay")]
    pub is_all_day: bool,
    pub status: String,
    #[sqlx(rename = "htmlLink")]
    pub html_link: Option<String>,
}

/// `googleEventId ? upsert(...) : create(...)` dengan `select` di atas.
pub async fn save_new_event(state: &AppState, e: &NewEvent) -> sqlx::Result<SavedEvent> {
    let conflict = if e.google_event_id.is_some() {
        r#" ON CONFLICT ("googleEventId", "companyId", "calendarId") DO UPDATE SET
             title = EXCLUDED.title, description = EXCLUDED.description,
             location = EXCLUDED.location, "startAt" = EXCLUDED."startAt",
             "endAt" = EXCLUDED."endAt", "isAllDay" = EXCLUDED."isAllDay",
             status = EXCLUDED.status, "htmlLink" = EXCLUDED."htmlLink",
             "rawMeta" = EXCLUDED."rawMeta", source = EXCLUDED.source,
             "syncedAt" = EXCLUDED."syncedAt", "updatedAt" = EXCLUDED."syncedAt""#
    } else {
        ""
    };
    let sql = format!(
        r#"INSERT INTO "googleCalendarEvent"
             ("companyId", "userId", "createdByUserId", "calendarId", "googleEventId", title,
              description, location, "startAt", "endAt", "isAllDay", status, "htmlLink",
              "rawMeta", source, "syncedAt")
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'confirmed', $12, $13, $14, $15)
           {conflict}
           RETURNING uuid::text AS uuid, source, "googleEventId", title, description, location,
                     "startAt", "endAt", "isAllDay", status, "htmlLink""#
    );
    sqlx::query_as::<_, SavedEvent>(&sql)
        .bind(e.company_id)
        .bind(e.user_id)
        .bind(e.created_by)
        .bind(&e.calendar_id)
        .bind(&e.google_event_id)
        .bind(&e.title)
        .bind(&e.description)
        .bind(&e.location)
        .bind(e.start_at)
        .bind(e.end_at)
        .bind(e.is_all_day)
        .bind(&e.html_link)
        .bind(e.raw.as_ref().map(Json))
        .bind(e.source)
        .bind(now_naive())
        .fetch_one(&state.db)
        .await
}

/// Ringkasan `{ upserted, cancelled }`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct Counts {
    pub upserted: u64,
    pub cancelled: u64,
}

impl std::ops::AddAssign for Counts {
    fn add_assign(&mut self, o: Self) {
        self.upserted += o.upserted;
        self.cancelled += o.cancelled;
    }
}

/// `Fail::Msg` untuk tanggal; dipakai pemanggil yang mem-parse tanggal DTO.
pub fn parse_or_fail(s: &str, field: &str) -> GResult<NaiveDateTime> {
    parse_js_date(s).ok_or_else(|| Fail::Msg(invalid_date(field)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn item_mapping() {
        let it = json!({"id": "ev1", "summary": "S", "start": {"date": "2026-05-20"},
                        "end": {"date": "2026-05-21"}, "status": "cancelled"});
        let e = event_from_item(&it, Some(7)).unwrap().unwrap();
        assert_eq!(e.google_event_id, "ev1:u7");
        assert!(e.is_all_day && e.cancelled);
        assert_eq!(e.title, "S");
        let none = event_from_item(&json!({"id": "x", "start": {}}), None).unwrap();
        assert!(none.is_none());
        assert!(
            event_from_item(&json!({"summary": "no id"}), None)
                .unwrap()
                .is_none()
        );
        let bad = json!({"id": "x", "start": {"dateTime": "nope"}, "end": {"date": "2026-01-01"}});
        assert!(event_from_item(&bad, None).is_err());
        let timed = json!({"id": "y", "start": {"dateTime": "2026-05-20T09:00:00+07:00"},
                           "end": {"dateTime": "2026-05-20T10:00:00+07:00"}});
        let e = event_from_item(&timed, None).unwrap().unwrap();
        assert!(!e.is_all_day && !e.cancelled);
        assert_eq!(e.title, "(no title)");
        assert_eq!(e.status, "confirmed");
    }
}
