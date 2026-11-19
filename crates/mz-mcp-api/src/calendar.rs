//! Port `src/mcp-api/calendar/*` (`GET /mcp-api/calendar/events`).

use axum::Extension;
use axum::Json;
use axum::extract::State;
use axum::http::Uri;
use mz_admin_mcp::body::V;
use mz_core::time::{JsDate, v8_date};
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::common::envelope;
use crate::dto::{finish, opt_int, query_of, required_date_not_empty, str_of};
use crate::pat::PatUser;

const MAX_DATE_RANGE_DAYS: f64 = 366.0;
const DAY_MS: f64 = 86_400_000.0;

/// `extractAttendees(rawMeta)` (calendar.service.ts:10). Elemen `null` -> `TypeError` -> `Err`.
fn extract_attendees(raw_meta: Option<&Value>) -> Result<Vec<Value>, ApiError> {
    let Some(Value::Array(items)) = raw_meta.and_then(|m| m.get("attendees")) else {
        return Ok(Vec::new());
    };
    items
        .iter()
        .map(|a| {
            if a.is_null() {
                return Err(ApiError::internal(anyhow::anyhow!(
                    "Cannot read properties of null (reading 'email')"
                )));
            }
            // `(a['email'] as string) ?? null`
            let pick = |k: &str| match a.get(k) {
                None | Some(Value::Null) => Value::Null,
                Some(v) => v.clone(),
            };
            Ok(json!({
                "email": pick("email"),
                "name": pick("displayName"),
                "responseStatus": pick("responseStatus"),
            }))
        })
        .collect()
}

type EventRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    JsDate,
    JsDate,
    bool,
    String,
    Option<String>,
    Option<i32>,
    JsDate,
    Option<Value>,
);

/// Node: src/mcp-api/calendar/calendar.controller.ts:15 + calendar.service.ts:46 (`GET events`)
pub async fn h_events(
    State(state): State<AppState>,
    Extension(user): Extension<PatUser>,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let q = query_of(uri.query());
    // `ListCalendarEventsDto`.
    let mut v = V::default();
    required_date_not_empty(&mut v, "startDate", q.get("startDate"));
    required_date_not_empty(&mut v, "endDate", q.get("endDate"));
    v.opt_is_in("source", q.get("source"), &["company", "personal", "all"]);
    let skip = opt_int(&mut v, "skip", q.get("skip"), 0, None);
    let take = opt_int(&mut v, "take", q.get("take"), 1, Some(100));
    finish(v)?;

    let user_id = user.user_id;
    // `getUserContext`: company aktif (aturan bersama); tanpa company -> 400 (bukan 403).
    let company_id = mz_core::company::find_company_id(&state.db, user_id).await?;
    let Some(company_id) = company_id else {
        return Err(ApiError::bad_request(
            "User is not associated with a company",
        ));
    };

    let start = v8_date(str_of(&q, "startDate").unwrap_or_default());
    let end = v8_date(str_of(&q, "endDate").unwrap_or_default());
    if let (Some(s), Some(e)) = (start, end) {
        if s > e {
            return Err(ApiError::bad_request("startDate must be before endDate"));
        }
        #[allow(clippy::cast_precision_loss)]
        let diff_days =
            (JsDate(e).timestamp_millis() - JsDate(s).timestamp_millis()) as f64 / DAY_MS;
        if diff_days > MAX_DATE_RANGE_DAYS {
            return Err(ApiError::bad_request("Date range max 366 days"));
        }
    }
    // Tanggal tak terparse -> Prisma menolak `Invalid Date` (500).
    let (Some(start), Some(end)) = (start, end) else {
        return Err(ApiError::internal(anyhow::anyhow!("Invalid Date")));
    };

    let skip = skip.unwrap_or(0);
    let take = take.unwrap_or(20);
    let source = str_of(&q, "source").unwrap_or("all");

    let push_where = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(r#" WHERE g."companyId" = "#)
            .push_bind(company_id)
            .push(r#" AND g."startAt" >= "#)
            .push_bind(JsDate(start))
            .push(r#" AND g."endAt" <= "#)
            .push_bind(JsDate(end))
            .push(" AND g.status <> 'cancelled'");
        match source {
            "company" => {
                qb.push(r#" AND g."userId" IS NULL"#);
            }
            "personal" => {
                qb.push(r#" AND g."userId" = "#).push_bind(user_id);
            }
            _ => {
                qb.push(r#" AND (g."userId" IS NULL OR g."userId" = "#)
                    .push_bind(user_id)
                    .push(")");
            }
        }
    };

    let mut count =
        QueryBuilder::<Postgres>::new(r#"SELECT count(*) FROM "googleCalendarEvent" g"#);
    push_where(&mut count);
    let total: i64 = count.build_query_scalar().fetch_one(&state.db).await?;

    let mut list = QueryBuilder::<Postgres>::new(
        r#"SELECT g.uuid::text, g.title, g.description, g.location, g."startAt", g."endAt",
                  g."isAllDay", g.status, g."htmlLink", g."userId", g."syncedAt", g."rawMeta"
             FROM "googleCalendarEvent" g"#,
    );
    push_where(&mut list);
    list.push(r#" ORDER BY g."startAt" ASC, g.id ASC LIMIT "#)
        .push_bind(take)
        .push(" OFFSET ")
        .push_bind(skip);
    let rows: Vec<EventRow> = list.build_query_as().fetch_all(&state.db).await?;

    let mut data: Vec<Value> = Vec::with_capacity(rows.len());
    for (
        uuid,
        title,
        description,
        location,
        start_at,
        end_at,
        is_all_day,
        status,
        html_link,
        e_user,
        synced,
        raw_meta,
    ) in rows
    {
        data.push(json!({
            "id": uuid,
            "source": if e_user.is_some() { "personal" } else { "company" },
            "title": title,
            "description": description,
            "location": location,
            "startAt": start_at,
            "endAt": end_at,
            "isAllDay": is_all_day,
            "status": status,
            "htmlLink": html_link,
            "syncedAt": synced,
            "attendees": extract_attendees(raw_meta.as_ref())?,
        }));
    }
    Ok(Json(envelope(
        Value::Array(data),
        Some(json!({ "total": total, "skip": skip, "take": take })),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attendees_extraction() {
        assert!(extract_attendees(None).unwrap_or_default().is_empty());
        assert!(
            extract_attendees(Some(&json!({ "attendees": "x" })))
                .unwrap_or_default()
                .is_empty()
        );
        let r = extract_attendees(Some(&json!({
            "attendees": [{ "email": "a@b.c", "displayName": "A", "responseStatus": "accepted" }, {}, "str"]
        })))
        .unwrap_or_default();
        assert_eq!(
            r,
            vec![
                json!({ "email": "a@b.c", "name": "A", "responseStatus": "accepted" }),
                json!({ "email": null, "name": null, "responseStatus": null }),
                json!({ "email": null, "name": null, "responseStatus": null }),
            ]
        );
        assert!(extract_attendees(Some(&json!({ "attendees": [null] }))).is_err());
    }
}
