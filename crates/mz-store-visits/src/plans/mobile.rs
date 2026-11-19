//! Port `src/store-visits/mobile-store-visit-plans.service.ts` (§4.4) — jadwal harian sales
//! pemanggil + ringkasan per tanggal. Tanpa tulis, tanpa transaksi, tanpa zona waktu company.

use chrono::NaiveDate;
use mz_core::company::require_company_id;
use mz_core::jsfmt::num;
use mz_core::time::JsDate;
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Value, json};

use super::dto::ListMobilePlans;
use super::logic::{MAX_RANGE_DAYS, assert_plan_date, is_fulfilled, mobile_compare};
use super::service::visit_type_ref;
use crate::geo_util::get_distance;
use crate::plan_dates::{from_plan_date, plan_date_diff_days, to_plan_date, today_plan_date};

/// Kolom daftar harian §4.4.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MobilePlanRow {
    pub uuid: String,
    #[sqlx(rename = "sortOrder")]
    pub sort_order: Option<i32>,
    pub note: Option<String>,
    pub vt_uuid: Option<String>,
    pub vt_code: Option<String>,
    pub vt_name: Option<String>,
    pub vt_requires_order: Option<bool>,
    pub vt_requires_photo: Option<bool>,
    pub vt_requires_gps: Option<bool>,
    pub vt_is_active: Option<bool>,
    pub store_uuid: String,
    pub store_code: String,
    pub store_name: String,
    pub store_address: String,
    pub store_lat: f64,
    pub store_lng: f64,
    pub store_radius: Option<i32>,
    pub group_uuid: Option<String>,
    pub group_name: Option<String>,
    pub visit_uuid: Option<String>,
    pub visit_check_in_at: Option<JsDate>,
    pub visit_status: Option<String>,
    pub visit_state: Option<String>,
}

/// `toPlan(row, hasCoords, lat, lng)` (mobile) + jarak untuk pengurutan.
pub fn to_mobile_plan(row: &MobilePlanRow, coords: Option<(f64, f64)>) -> (Option<i64>, Value) {
    let fulfilled = is_fulfilled(row.visit_state.as_deref());
    let distance = coords.map(|(lat, lng)| get_distance(lat, lng, row.store_lat, row.store_lng));
    let group = match (&row.group_uuid, &row.group_name) {
        (Some(u), Some(n)) => json!({ "id": u, "name": n }),
        _ => Value::Null,
    };
    let visit = match (&row.visit_uuid, fulfilled) {
        (Some(u), true) => json!({
            "id": u,
            "checkInAt": row.visit_check_in_at.map(|d| d.timestamp_millis()),
            "status": row.visit_status,
        }),
        _ => Value::Null,
    };
    (
        distance,
        json!({
            "id": row.uuid,
            "sortOrder": row.sort_order,
            "note": row.note,
            "status": if fulfilled { "fulfilled" } else { "pending" },
            "visitType": visit_type_ref(&row.vt_uuid, &row.vt_code, &row.vt_name, (
                row.vt_requires_order, row.vt_requires_photo, row.vt_requires_gps, row.vt_is_active,
            )),
            "store": {
                "id": row.store_uuid,
                "code": row.store_code,
                "name": row.store_name,
                "address": row.store_address,
                "lat": num(row.store_lat),
                "lng": num(row.store_lng),
                "radiusMeters": row.store_radius,
                "distanceMeters": distance,
                "group": group,
            },
            "visit": visit,
        }),
    )
}

/// §4.4 `findAll(userId, query)` → `{ date, summary, data }`.
pub async fn find_all(state: &AppState, user_id: i32, q: &ListMobilePlans) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    if q.lat.is_some() != q.lng.is_some() {
        return Err(ApiError::bad_request(
            "lat and lng must be sent together, or not at all",
        ));
    }
    let coords = q.lat.zip(q.lng);
    let date = match &q.date {
        Some(d) => assert_plan_date(Some(d), "date")?,
        None => today_plan_date(),
    };
    let visit_date = to_plan_date(&date).unwrap_or_default();
    let rows: Vec<MobilePlanRow> = sqlx::query_as(
        r#"SELECT p.uuid::text AS uuid, p."sortOrder", p.note,
                  vt.uuid::text AS vt_uuid, vt.code AS vt_code, vt.name AS vt_name,
                  vt."requiresOrder" AS vt_requires_order, vt."requiresPhoto" AS vt_requires_photo,
                  vt."requiresGps" AS vt_requires_gps, vt."isActive" AS vt_is_active,
                  s.uuid::text AS store_uuid, s.code AS store_code, s.name AS store_name,
                  s.address AS store_address, s.lat AS store_lat, s.lng AS store_lng,
                  s."radiusMeters" AS store_radius,
                  g.uuid::text AS group_uuid, g.name AS group_name,
                  v.uuid::text AS visit_uuid, v."checkInAt" AS visit_check_in_at,
                  v.status::text AS visit_status, v.state::text AS visit_state
             FROM "storeVisitPlan" p
             JOIN store s ON s.id = p."storeId"
             LEFT JOIN "storeGroup" g ON g.id = s."groupId"
             LEFT JOIN "visitType" vt ON vt.id = p."visitTypeId"
             LEFT JOIN "storeVisit" v ON v."planId" = p.id
            WHERE p."companyId" = $1 AND p."userId" = $2 AND p."deletedAt" IS NULL AND p."visitDate" = $3
              AND s."deletedAt" IS NULL AND s."isActive" = true
            ORDER BY p.id"#,
    )
    .bind(company_id)
    .bind(user_id)
    .bind(visit_date)
    .fetch_all(&state.db)
    .await?;
    let mut data: Vec<(Option<i64>, Option<i32>, String, Value)> = rows
        .iter()
        .map(|r| {
            let (d, v) = to_mobile_plan(r, coords);
            (d, r.sort_order, r.store_name.clone(), v)
        })
        .collect();
    data.sort_by(|a, b| {
        mobile_compare(
            coords.is_some(),
            (a.0, a.1, a.2.as_str()),
            (b.0, b.1, b.2.as_str()),
        )
    });
    let planned = data.len();
    let fulfilled = data
        .iter()
        .filter(|x| x.3.get("status").and_then(Value::as_str) == Some("fulfilled"))
        .count();
    Ok(json!({
        "date": date,
        "summary": { "planned": planned, "fulfilled": fulfilled, "remaining": planned - fulfilled },
        "data": data.into_iter().map(|x| x.3).collect::<Vec<_>>(),
    }))
}

/// §4.4 `summary(userId, query)` → `{ data: [{ date, planned, fulfilled, remaining }] }`.
pub async fn summary(state: &AppState, user_id: i32, start: &str, end: &str) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let start_date = assert_plan_date(Some(start), "startDate")?;
    let end_date = assert_plan_date(Some(end), "endDate")?;
    let span = plan_date_diff_days(&start_date, &end_date).unwrap_or(0);
    if span < 0 {
        return Err(ApiError::coded(
            axum::http::StatusCode::BAD_REQUEST,
            "INVALID_DATE_RANGE",
            "startDate must be on or before endDate",
        ));
    }
    if span + 1 > MAX_RANGE_DAYS {
        return Err(ApiError::coded(
            axum::http::StatusCode::BAD_REQUEST,
            "INVALID_DATE_RANGE",
            format!("Date range is limited to {MAX_RANGE_DAYS} days"),
        ));
    }
    let rows: Vec<(NaiveDate, i64, i64)> = sqlx::query_as(
        r#"SELECT p."visitDate", COUNT(*),
                  COUNT(*) FILTER (WHERE v.id IS NOT NULL AND v.state <> 'cancelled'::"StoreVisitState")
             FROM "storeVisitPlan" p LEFT JOIN "storeVisit" v ON v."planId" = p.id
            WHERE p."companyId" = $1 AND p."userId" = $2 AND p."deletedAt" IS NULL
              AND p."visitDate" >= $3 AND p."visitDate" <= $4
            GROUP BY p."visitDate" ORDER BY p."visitDate""#,
    )
    .bind(company_id)
    .bind(user_id)
    .bind(to_plan_date(&start_date).unwrap_or_default())
    .bind(to_plan_date(&end_date).unwrap_or_default())
    .fetch_all(&state.db)
    .await?;
    let data: Vec<Value> = rows
        .iter()
        .map(|(d, total, done)| {
            json!({ "date": from_plan_date(*d), "planned": total, "fulfilled": done, "remaining": total - done })
        })
        .collect();
    Ok(json!({ "data": data }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, sort: Option<i32>, lat: f64, state: Option<&str>) -> MobilePlanRow {
        MobilePlanRow {
            uuid: format!("p-{name}"),
            sort_order: sort,
            note: None,
            vt_uuid: None,
            vt_code: None,
            vt_name: None,
            vt_requires_order: None,
            vt_requires_photo: None,
            vt_requires_gps: None,
            vt_is_active: None,
            store_uuid: "s".into(),
            store_code: "S".into(),
            store_name: name.into(),
            store_address: "Jl".into(),
            store_lat: lat,
            store_lng: 110.0,
            store_radius: None,
            group_uuid: None,
            group_name: None,
            visit_uuid: state.map(|_| "v".to_owned()),
            visit_check_in_at: JsDate::from_millis(9),
            visit_status: Some("success".into()),
            visit_state: state.map(str::to_owned),
        }
    }

    /// "status fulfilled/pending", "cancelled → pending", "tanpa koordinat distanceMeters null",
    /// "group null kalau toko tidak punya region".
    #[test]
    fn mobile_shape() {
        let (d, v) = to_mobile_plan(&row("A", None, -7.0, Some("completed")), None);
        assert_eq!(d, None);
        assert_eq!(v["status"], "fulfilled");
        assert_eq!(
            v["visit"],
            json!({"id": "v", "checkInAt": 9, "status": "success"})
        );
        assert_eq!(v["store"]["distanceMeters"], Value::Null);
        assert_eq!(v["store"]["group"], Value::Null);
        assert_eq!(v["store"]["lng"], json!(110));
        let (_, v) = to_mobile_plan(&row("A", None, -7.0, Some("cancelled")), None);
        assert_eq!(v["status"], "pending");
        assert_eq!(v["visit"], Value::Null);
        let (d, _) = to_mobile_plan(&row("A", None, -7.0, None), Some((-7.0, 110.0)));
        assert_eq!(d, Some(0));
    }
}
