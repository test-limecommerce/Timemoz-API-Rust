//! Port `src/store-visits/store-visit-events.service.ts` (docs/api-mobile-store-visit-event.md):
//! ping posisi manual (§3.1, dibatasi `PlanLimit::AutoCheckIns`), baris bookend enter/exit dari
//! check-in/check-out (§3.2, best-effort, tidak dibatasi paket), dan riwayat milik caller.

use axum::http::StatusCode;
use chrono::NaiveDateTime;
use mz_core::jsfmt::num;
use mz_core::time::{JsDate, v8_date};
use mz_core::{ApiError, ApiResult, AppState};
use mz_sales_geo::plan::{PlanLimit, assert_plan_limit};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::geo_util::{effective_radius, get_distance};
use crate::visits::dto::{EventHistoryQuery, StoreVisitEvent};
use crate::visits::service::visit_not_found;

/// `distance <= radius` → `enter`, selain itu `exit`.
pub fn classify_event(distance_meters: i64, radius_meters: i32) -> &'static str {
    if distance_meters <= i64::from(radius_meters) {
        "enter"
    } else {
        "exit"
    }
}

/// `new Date(ms)` JS (TimeClip memotong pecahan); di luar rentang → `None` (Invalid Date → 500).
pub fn js_date_from_ms_f64(ms: f64) -> Option<NaiveDateTime> {
    if !ms.is_finite() || ms.abs() > 8.64e15 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    JsDate::from_millis(ms.trunc() as i64).map(|d| d.0)
}

#[derive(sqlx::FromRow)]
struct ActiveVisitStore {
    id: i32,
    #[sqlx(rename = "companyId")]
    company_id: i32,
    store_id: i32,
    store_uuid: String,
    store_name: String,
    store_lat: f64,
    store_lng: f64,
    store_radius: Option<i32>,
}

/// §3.1 `logEvent(userId, dto)` → `{ logged, event, store: {id, name}, distanceMeters }`.
/// 409 `NO_ACTIVE_VISIT`; 402 batas `AUTO_CHECKIN_LIMIT_REACHED`.
pub async fn log_event(state: &AppState, user_id: i32, dto: &StoreVisitEvent) -> ApiResult<Value> {
    let visit: Option<ActiveVisitStore> = sqlx::query_as(
        r#"SELECT v.id, v."companyId", s.id AS store_id, s.uuid::text AS store_uuid, s.name AS store_name,
                  s.lat AS store_lat, s.lng AS store_lng, s."radiusMeters" AS store_radius
             FROM "storeVisit" v JOIN store s ON s.id = v."storeId"
            WHERE v."userId" = $1 AND v.state = 'active'::"StoreVisitState"
            ORDER BY v.id LIMIT 1"#,
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(visit) = visit else {
        return Err(ApiError::coded(
            StatusCode::CONFLICT,
            "NO_ACTIVE_VISIT",
            "No active store visit for this user",
        ));
    };
    let default_radius: Option<i32> =
        sqlx::query_scalar(r#"SELECT "storeVisitDefaultRadiusMeters" FROM company WHERE id = $1"#)
            .bind(visit.company_id)
            .fetch_optional(&state.db)
            .await?;
    let radius = effective_radius(visit.store_radius, default_radius.unwrap_or(0));
    let distance = get_distance(dto.lat, dto.lng, visit.store_lat, visit.store_lng);
    let event = classify_event(distance, radius);
    let timestamp = js_date_from_ms_f64(dto.timestamp)
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("Invalid Date for timestamp")))?;

    let mut tx = state.db.begin().await?;
    assert_plan_limit(&mut tx, visit.company_id, PlanLimit::AutoCheckIns).await?;
    sqlx::query(
        r#"INSERT INTO "storeVisitLocationEvent"
             ("visitId", "storeId", "userId", event, lat, lng, accuracy, timestamp, source)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'ping')"#,
    )
    .bind(visit.id)
    .bind(visit.store_id)
    .bind(user_id)
    .bind(event)
    .bind(dto.lat)
    .bind(dto.lng)
    .bind(dto.accuracy)
    .bind(timestamp)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(json!({
        "logged": true,
        "event": event,
        "store": { "id": visit.store_uuid, "name": visit.store_name },
        "distanceMeters": distance,
    }))
}

/// Parameter `logBookendEvent`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BookendEvent {
    pub visit_id: i32,
    pub store_id: i32,
    pub user_id: i32,
    pub event: &'static str,
    pub lat: f64,
    pub lng: f64,
    pub accuracy: f64,
    pub at: NaiveDateTime,
}

/// §3.2 `logBookendEvent`: `enter` idempoten per kunjungan, `exit` tanpa cek. Best-effort.
pub async fn log_bookend_event(db: &PgPool, p: &BookendEvent) {
    let res: sqlx::Result<()> = async {
        if p.event == "enter" {
            let existing: Option<i32> = sqlx::query_scalar(
                r#"SELECT id FROM "storeVisitLocationEvent" WHERE "visitId" = $1 AND event = 'enter' LIMIT 1"#,
            )
            .bind(p.visit_id)
            .fetch_optional(db)
            .await?;
            if existing.is_some() {
                return Ok(());
            }
        }
        sqlx::query(
            r#"INSERT INTO "storeVisitLocationEvent"
                 ("visitId", "storeId", "userId", event, lat, lng, accuracy, timestamp, source)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'bookend')"#,
        )
        .bind(p.visit_id)
        .bind(p.store_id)
        .bind(p.user_id)
        .bind(p.event)
        .bind(p.lat)
        .bind(p.lng)
        .bind(p.accuracy)
        .bind(p.at)
        .execute(db)
        .await?;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        tracing::error!(
            "storeVisitLocationEvent ({}) failed for visit {}: {e}",
            p.event,
            p.visit_id
        );
    }
}

/// `new Date(\`${d}T00:00:00.000Z\`)` dsb.; Invalid Date → 500 (galat Prisma di Node).
pub fn day_edge(date: &str, suffix: &str) -> ApiResult<NaiveDateTime> {
    v8_date(&format!("{date}{suffix}"))
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("Invalid Date: {date}{suffix}")))
}

/// `Math.ceil(total / limit)`.
pub fn total_pages(total: i64, limit: i64) -> i64 {
    if limit <= 0 {
        return 0;
    }
    (total + limit - 1).div_euclid(limit)
}

#[derive(sqlx::FromRow)]
struct EventRow {
    id: i32,
    visit_uuid: String,
    store_uuid: String,
    store_name: String,
    event: String,
    lat: f64,
    lng: f64,
    accuracy: f64,
    timestamp: JsDate,
}

/// §1 `listEventHistory(userId, query)` → `{ data, meta }`.
pub async fn list_event_history(
    state: &AppState,
    user_id: i32,
    q: &EventHistoryQuery,
) -> ApiResult<Value> {
    let company_id = mz_core::company::require_company_id(&state.db, user_id).await?;
    let visit_id = match &q.visit_id {
        Some(uuid) => {
            let id: Option<i32> = sqlx::query_scalar(
                r#"SELECT id FROM "storeVisit" WHERE uuid = $1::uuid AND "userId" = $2 AND "companyId" = $3 LIMIT 1"#,
            )
            .bind(uuid)
            .bind(user_id)
            .bind(company_id)
            .fetch_optional(&state.db)
            .await?;
            Some(id.ok_or_else(visit_not_found)?)
        }
        None => None,
    };
    let gte = q
        .start_date
        .as_deref()
        .map(|d| day_edge(d, "T00:00:00.000Z"))
        .transpose()?;
    let lte = q
        .end_date
        .as_deref()
        .map(|d| day_edge(d, "T23:59:59.999Z"))
        .transpose()?;
    if let (Some(g), Some(l)) = (gte, lte)
        && l < g
    {
        return Err(ApiError::bad_request(
            "endDate must be on or after startDate",
        ));
    }

    let push_where = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(r#" WHERE e."userId" = "#).push_bind(user_id);
        match visit_id {
            Some(id) => {
                qb.push(r#" AND e."visitId" = "#).push_bind(id);
            }
            None => {
                qb.push(r#" AND v."companyId" = "#).push_bind(company_id);
            }
        }
        if let Some(ev) = &q.event {
            qb.push(" AND e.event = ").push_bind(ev.clone());
        }
        if let Some(g) = gte {
            qb.push(" AND e.timestamp >= ").push_bind(g);
        }
        if let Some(l) = lte {
            qb.push(" AND e.timestamp <= ").push_bind(l);
        }
    };
    const FROM: &str = r#" FROM "storeVisitLocationEvent" e
        JOIN "storeVisit" v ON v.id = e."visitId"
        JOIN store s ON s.id = e."storeId""#;

    let mut qb = QueryBuilder::<Postgres>::new(
        r#"SELECT e.id, v.uuid::text AS visit_uuid, s.uuid::text AS store_uuid, s.name AS store_name,
                  e.event, e.lat, e.lng, e.accuracy, e.timestamp"#,
    );
    qb.push(FROM);
    push_where(&mut qb);
    qb.push(" ORDER BY e.timestamp DESC, e.id DESC LIMIT ")
        .push_bind(q.limit)
        .push(" OFFSET ")
        .push_bind((q.page - 1) * q.limit);
    let rows: Vec<EventRow> = qb.build_query_as().fetch_all(&state.db).await?;

    let mut cq = QueryBuilder::<Postgres>::new("SELECT COUNT(*)");
    cq.push(FROM);
    push_where(&mut cq);
    let total: i64 = cq.build_query_scalar().fetch_one(&state.db).await?;

    let data: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.id,
                "visitId": r.visit_uuid,
                "storeId": r.store_uuid,
                "storeName": r.store_name,
                "event": r.event,
                "lat": num(r.lat),
                "lng": num(r.lng),
                "accuracy": num(r.accuracy),
                "timestamp": r.timestamp,
            })
        })
        .collect();
    Ok(json!({
        "data": data,
        "meta": { "total": total, "page": q.page, "limit": q.limit, "totalPages": total_pages(total, q.limit) },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "treats distance exactly at the radius as enter" / "logs exit when outside".
    #[test]
    fn enter_exit() {
        assert_eq!(classify_event(100, 100), "enter");
        assert_eq!(classify_event(101, 100), "exit");
        assert_eq!(classify_event(0, effective_radius(None, 150)), "enter");
    }

    #[test]
    fn js_dates_and_pages() {
        assert_eq!(
            js_date_from_ms_f64(1.9).map(|d| d.and_utc().timestamp_millis()),
            Some(1)
        );
        assert_eq!(js_date_from_ms_f64(9e15), None);
        assert_eq!(total_pages(0, 20), 0);
        assert_eq!(total_pages(41, 20), 3);
        assert_eq!(
            day_edge("2026-02-31", "T00:00:00.000Z")
                .unwrap()
                .to_string(),
            "2026-03-03 00:00:00"
        );
        assert!(day_edge("2026-01-01T00:00:00Z", "T00:00:00.000Z").is_err());
    }
}
