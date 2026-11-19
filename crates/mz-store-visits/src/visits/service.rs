//! Port `src/store-visits/store-visits.service.ts` — validate-location (§6.3), check-in (§6.4),
//! check-out (§6.7), cancel (§6.11), kunjungan aktif (§6.6), riwayat (§6.8), detail (§6.9),
//! rekap harian (§6.10), saran toko berikutnya, audit percobaan (§9.8).
//!
//! Aturan keputusan dipisah sebagai fungsi murni (`*_rules`, `*_verdict`, …) supaya kasus
//! `store-visits.service.spec.ts` bisa diuji tanpa DB.

use std::collections::HashMap;

use axum::http::StatusCode;
use chrono::{Duration, NaiveDateTime, Utc};
use mz_audit::{AuditHandle, Lookup, Model};
use mz_core::jsfmt::{num, trim};
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Map, Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::enums::{GpsValidation, StoreVisitAuditEvent, StoreVisitMethod, StoreVisitStatus};
use crate::geo_util::{
    DEFAULT_RADIUS_KM, MAX_PREFILTER_ROWS, bounding_box, effective_radius, get_distance,
};
use crate::photos::{
    REQUIRED_PHOTO_KINDS, confirmed_photo_counts, confirmed_photo_status, photos_of_visits,
    serialize_photos,
};
use crate::plan_dates::{parse_plan_date, today_plan_date};
use crate::scope::{
    ScopeDenial, ScopePosition, ScopeStore, StoreScope, assert_in_scope, deny_out_of_scope,
    is_in_scope, push_scope_clause, resolve_store_scope,
};
use crate::visits::attendance::{
    AttendanceVisitSource, local_date, local_day_bounds, open_shift_for_visit,
};
use crate::visits::dto::{CheckIn, CheckOut, ListVisitsQuery, ValidateLocation};
use crate::visits::events::{BookendEvent, day_edge, log_bookend_event, total_pages};

// ───────────────────────── galat bersama ─────────────────────────

/// 404 `VISIT_NOT_FOUND`.
pub fn visit_not_found() -> ApiError {
    ApiError::coded(StatusCode::NOT_FOUND, "VISIT_NOT_FOUND", "Visit not found")
}

/// 409 `VISIT_NOT_ACTIVE`.
pub fn visit_not_active() -> ApiError {
    ApiError::coded(
        StatusCode::CONFLICT,
        "VISIT_NOT_ACTIVE",
        "This visit is no longer active",
    )
}

/// 409 `VISIT_ALREADY_ACTIVE`.
pub fn visit_already_active() -> ApiError {
    ApiError::coded(
        StatusCode::CONFLICT,
        "VISIT_ALREADY_ACTIVE",
        "You already have an active visit, check out from it first",
    )
}

fn unprocessable(code: &str, message: impl Into<String>) -> ApiError {
    ApiError::coded(StatusCode::UNPROCESSABLE_ENTITY, code, message)
}

fn internal(msg: impl std::fmt::Display) -> ApiError {
    ApiError::internal(anyhow::anyhow!("{msg}"))
}

fn ms(d: NaiveDateTime) -> i64 {
    d.and_utc().timestamp_millis()
}

fn opt_ms(d: Option<NaiveDateTime>) -> Value {
    d.map_or(Value::Null, |d| Value::from(ms(d)))
}

fn opt_num(v: Option<f64>) -> Value {
    v.map_or(Value::Null, num)
}

// ───────────────────────── pengaturan company ─────────────────────────

/// `COMPANY_SETTINGS_SELECT` (§5.5).
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct CompanySettings {
    #[sqlx(rename = "storeVisitDefaultRadiusMeters")]
    pub default_radius_meters: i32,
    #[sqlx(rename = "storeVisitMaxAccuracyMeters")]
    pub max_accuracy_meters: i32,
    #[sqlx(rename = "storeVisitBlockMockLocation")]
    pub block_mock_location: bool,
    #[sqlx(rename = "storeVisitRequirePhotos")]
    pub require_photos: bool,
    #[sqlx(rename = "storeVisitAllowOverride")]
    pub allow_override: bool,
    #[sqlx(rename = "storeVisitEnforceGroupScope")]
    pub enforce_group_scope: bool,
    #[sqlx(rename = "storeVisitMinDurationMinutes")]
    pub min_duration_minutes: i32,
    #[sqlx(rename = "storeVisitCountsAsAttendance")]
    pub counts_as_attendance: bool,
    #[sqlx(rename = "timeZone")]
    pub time_zone: Option<String>,
}

impl CompanySettings {
    /// `settings.timeZone || 'UTC'`.
    pub fn zone(&self) -> &str {
        self.time_zone
            .as_deref()
            .filter(|z| !z.is_empty())
            .unwrap_or("UTC")
    }
}

/// `getCompanySettings(companyId)`: 403 `No company found for user` bila baris tidak ada.
pub async fn get_company_settings(db: &PgPool, company_id: i32) -> ApiResult<CompanySettings> {
    let row: Option<CompanySettings> = sqlx::query_as(
        r#"SELECT "storeVisitDefaultRadiusMeters", "storeVisitMaxAccuracyMeters", "storeVisitBlockMockLocation",
                  "storeVisitRequirePhotos", "storeVisitAllowOverride", "storeVisitEnforceGroupScope",
                  "storeVisitMinDurationMinutes", "storeVisitCountsAsAttendance", "timeZone"
             FROM company WHERE id = $1"#,
    )
    .bind(company_id)
    .fetch_optional(db)
    .await?;
    row.ok_or_else(|| ApiError::forbidden("No company found for user"))
}

// ───────────────────────── baris kunjungan ─────────────────────────

/// `storeVisit` + toko + visit type snapshot + jadwal yang diklaim (beserta tipe jadwal).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct VisitFull {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    #[sqlx(rename = "storeId")]
    pub store_id: i32,
    #[sqlx(rename = "userId")]
    pub user_id: i32,
    #[sqlx(rename = "visitNumber")]
    pub visit_number: String,
    pub state: String,
    pub status: Option<String>,
    pub note: Option<String>,
    pub tags: Vec<String>,
    #[sqlx(rename = "durationSeconds")]
    pub duration_seconds: Option<i32>,
    #[sqlx(rename = "checkInAt")]
    pub check_in_at: NaiveDateTime,
    #[sqlx(rename = "checkInClientAt")]
    pub check_in_client_at: Option<NaiveDateTime>,
    #[sqlx(rename = "checkInLat")]
    pub check_in_lat: f64,
    #[sqlx(rename = "checkInLng")]
    pub check_in_lng: f64,
    #[sqlx(rename = "checkInAccuracy")]
    pub check_in_accuracy: f64,
    #[sqlx(rename = "checkInDistance")]
    pub check_in_distance: i32,
    #[sqlx(rename = "checkInMethod")]
    pub check_in_method: String,
    #[sqlx(rename = "checkInGpsValidation")]
    pub check_in_gps_validation: String,
    #[sqlx(rename = "checkInIsMock")]
    pub check_in_is_mock: bool,
    #[sqlx(rename = "checkInIsOverride")]
    pub check_in_is_override: bool,
    #[sqlx(rename = "checkInOverrideReason")]
    pub check_in_override_reason: Option<String>,
    #[sqlx(rename = "checkOutAt")]
    pub check_out_at: Option<NaiveDateTime>,
    #[sqlx(rename = "checkOutClientAt")]
    pub check_out_client_at: Option<NaiveDateTime>,
    #[sqlx(rename = "checkOutLat")]
    pub check_out_lat: Option<f64>,
    #[sqlx(rename = "checkOutLng")]
    pub check_out_lng: Option<f64>,
    #[sqlx(rename = "checkOutAccuracy")]
    pub check_out_accuracy: Option<f64>,
    #[sqlx(rename = "checkOutDistance")]
    pub check_out_distance: Option<i32>,
    #[sqlx(rename = "checkOutGpsValidation")]
    pub check_out_gps_validation: Option<String>,
    #[sqlx(rename = "createdAt")]
    pub created_at: NaiveDateTime,
    pub store_uuid: String,
    pub store_code: String,
    pub store_name: String,
    pub store_owner_name: String,
    pub store_lat: f64,
    pub store_lng: f64,
    pub store_radius_meters: Option<i32>,
    pub vt_uuid: Option<String>,
    pub vt_code: Option<String>,
    pub vt_name: Option<String>,
    pub vt_requires_order: Option<bool>,
    pub vt_requires_photo: Option<bool>,
    pub vt_requires_gps: Option<bool>,
    pub vt_is_active: Option<bool>,
    pub plan_uuid: Option<String>,
    pub plan_note: Option<String>,
    pub pvt_uuid: Option<String>,
    pub pvt_code: Option<String>,
    pub pvt_name: Option<String>,
    pub pvt_requires_order: Option<bool>,
    pub pvt_requires_photo: Option<bool>,
    pub pvt_requires_gps: Option<bool>,
    pub pvt_is_active: Option<bool>,
}

/// `SELECT` [`VisitFull`] (alias `v`, `s`, `vt`, `p`, `pvt`); tambahkan `WHERE`.
pub const VISIT_FULL_SELECT: &str = r#"SELECT v.id, v.uuid::text AS uuid, v."companyId", v."storeId", v."userId",
       v."visitNumber", v.state::text AS state, v.status::text AS status, v.note, v.tags, v."durationSeconds",
       v."checkInAt", v."checkInClientAt", v."checkInLat", v."checkInLng", v."checkInAccuracy",
       v."checkInDistance", v."checkInMethod"::text AS "checkInMethod",
       v."checkInGpsValidation"::text AS "checkInGpsValidation", v."checkInIsMock", v."checkInIsOverride",
       v."checkInOverrideReason", v."checkOutAt", v."checkOutClientAt", v."checkOutLat", v."checkOutLng",
       v."checkOutAccuracy", v."checkOutDistance", v."checkOutGpsValidation"::text AS "checkOutGpsValidation",
       v."createdAt",
       s.uuid::text AS store_uuid, s.code AS store_code, s.name AS store_name, s."ownerName" AS store_owner_name,
       s.lat AS store_lat, s.lng AS store_lng, s."radiusMeters" AS store_radius_meters,
       vt.uuid::text AS vt_uuid, vt.code AS vt_code, vt.name AS vt_name, vt."requiresOrder" AS vt_requires_order,
       vt."requiresPhoto" AS vt_requires_photo, vt."requiresGps" AS vt_requires_gps, vt."isActive" AS vt_is_active,
       p.uuid::text AS plan_uuid, p.note AS plan_note,
       pvt.uuid::text AS pvt_uuid, pvt.code AS pvt_code, pvt.name AS pvt_name,
       pvt."requiresOrder" AS pvt_requires_order, pvt."requiresPhoto" AS pvt_requires_photo,
       pvt."requiresGps" AS pvt_requires_gps, pvt."isActive" AS pvt_is_active
  FROM "storeVisit" v
  JOIN store s ON s.id = v."storeId"
  LEFT JOIN "visitType" vt ON vt.id = v."visitTypeId"
  LEFT JOIN "storeVisitPlan" p ON p.id = v."planId"
  LEFT JOIN "visitType" pvt ON pvt.id = p."visitTypeId""#;

#[allow(clippy::too_many_arguments)]
fn visit_type_ref(
    uuid: &Option<String>,
    code: &Option<String>,
    name: &Option<String>,
    ro: Option<bool>,
    rp: Option<bool>,
    rg: Option<bool>,
    active: Option<bool>,
) -> Value {
    match (uuid, code, name) {
        (Some(u), Some(c), Some(n)) => json!({
            "id": u, "code": c, "name": n,
            "requiresOrder": ro.unwrap_or(false),
            "requiresPhoto": rp.unwrap_or(false),
            "requiresGps": rg.unwrap_or(true),
            "isActive": active.unwrap_or(true),
        }),
        _ => Value::Null,
    }
}

impl VisitFull {
    /// `toVisitTypeRef(visit.visitType)` atau `null`.
    pub fn visit_type_json(&self) -> Value {
        visit_type_ref(
            &self.vt_uuid,
            &self.vt_code,
            &self.vt_name,
            self.vt_requires_order,
            self.vt_requires_photo,
            self.vt_requires_gps,
            self.vt_is_active,
        )
    }

    /// `plan: { id, note, visitType } | null` (§4.5, §7.2).
    pub fn plan_json(&self) -> Value {
        match &self.plan_uuid {
            None => Value::Null,
            Some(u) => json!({
                "id": u,
                "note": self.plan_note,
                "visitType": visit_type_ref(
                    &self.pvt_uuid, &self.pvt_code, &self.pvt_name, self.pvt_requires_order,
                    self.pvt_requires_photo, self.pvt_requires_gps, self.pvt_is_active,
                ),
            }),
        }
    }

    /// `toStoreRef`.
    pub fn store_ref(&self) -> Value {
        json!({ "id": self.store_uuid, "code": self.store_code, "name": self.store_name })
    }

    fn attendance_source(&self) -> AttendanceVisitSource {
        AttendanceVisitSource {
            user_id: self.user_id,
            company_id: self.company_id,
            visit_number: self.visit_number.clone(),
            check_in_at: self.check_in_at,
            check_in_client_at: self.check_in_client_at,
            check_in_lat: self.check_in_lat,
            check_in_lng: self.check_in_lng,
            check_in_accuracy: self.check_in_accuracy,
            check_in_is_mock: self.check_in_is_mock,
            check_in_gps_validation: self.check_in_gps_validation.clone(),
        }
    }
}

/// Satu kunjungan dengan filter bebas (nilai lewat `push_bind`).
pub async fn find_visit_full(
    db: &PgPool,
    filter: impl FnOnce(&mut QueryBuilder<'_, Postgres>),
) -> ApiResult<Option<VisitFull>> {
    let mut qb = QueryBuilder::<Postgres>::new(VISIT_FULL_SELECT);
    qb.push(" WHERE ");
    filter(&mut qb);
    qb.push(" ORDER BY v.id LIMIT 1");
    Ok(qb.build_query_as().fetch_optional(db).await?)
}

// ───────────────────────── aturan murni ─────────────────────────

/// `toCheckOutWindow(settings, checkInAt)` → `(minDurationMinutes, checkOutAllowedAt ms | null)`.
pub fn check_out_window(
    settings: &CompanySettings,
    check_in_at: NaiveDateTime,
) -> (i32, Option<i64>) {
    let m = settings.min_duration_minutes;
    (m, (m > 0).then(|| ms(check_in_at) + i64::from(m) * 60_000))
}

fn insert_window(
    obj: &mut Map<String, Value>,
    settings: &CompanySettings,
    check_in_at: NaiveDateTime,
) {
    let (m, at) = check_out_window(settings, check_in_at);
    obj.insert("minDurationMinutes".into(), Value::from(m));
    obj.insert(
        "checkOutAllowedAt".into(),
        at.map_or(Value::Null, Value::from),
    );
}

/// §6.3 `validateLocation` verdict (tanpa I/O).
pub fn validate_location_verdict(
    settings: &CompanySettings,
    distance_meters: i64,
    radius_meters: i32,
    accuracy_meters: f64,
    is_mock: bool,
) -> Value {
    let verdict = |gps: GpsValidation, can_check_in: bool, can_override: bool, message: String| {
        json!({
            "gpsValidation": gps,
            "distanceMeters": distance_meters,
            "canCheckIn": can_check_in,
            "canRequestOverride": can_override,
            "message": message,
        })
    };
    if accuracy_meters > f64::from(settings.max_accuracy_meters) {
        return verdict(
            GpsValidation::lowAccuracy,
            false,
            false,
            "GPS accuracy is too low to verify your location".into(),
        );
    }
    if is_mock && settings.block_mock_location {
        return verdict(
            GpsValidation::mock,
            false,
            false,
            "Mock location is not allowed".into(),
        );
    }
    if distance_meters > i64::from(radius_meters) {
        return verdict(
            GpsValidation::outside,
            false,
            settings.allow_override,
            format!("You are {distance_meters} m from the store, outside the allowed area"),
        );
    }
    verdict(
        if is_mock {
            GpsValidation::mock
        } else {
            GpsValidation::valid
        },
        true,
        false,
        "Location verified".into(),
    )
}

/// Hasil aturan check-in §5.2.3–§5.2.5.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckInRule {
    /// 422 `VISIT_GPS_ACCURACY_TOO_LOW` (tanpa audit).
    AccuracyTooLow,
    /// Audit `mockLocationBlocked`, lalu 422 `VISIT_MOCK_LOCATION_BLOCKED`.
    MockBlocked,
    /// Audit `outOfRadius`, lalu 422 `VISIT_OVERRIDE_NOT_ALLOWED` (`override_not_allowed`) atau
    /// `VISIT_OUT_OF_RADIUS`.
    OutOfRadius { override_not_allowed: bool },
    /// Lolos.
    Accepted {
        gps_validation: GpsValidation,
        used_override: bool,
        has_gps_issue: bool,
    },
}

/// Aturan check-in murni.
pub fn check_in_rules(
    settings: &CompanySettings,
    distance_meters: i64,
    radius_meters: i32,
    accuracy_meters: f64,
    is_mock: bool,
    wants_override: bool,
    override_reason: Option<&str>,
) -> CheckInRule {
    if accuracy_meters > f64::from(settings.max_accuracy_meters) {
        return CheckInRule::AccuracyTooLow;
    }
    if is_mock && settings.block_mock_location {
        return CheckInRule::MockBlocked;
    }
    let is_outside = distance_meters > i64::from(radius_meters);
    let mut used_override = false;
    if is_outside {
        let granted = wants_override && settings.allow_override && override_reason.is_some();
        if !granted {
            return CheckInRule::OutOfRadius {
                override_not_allowed: wants_override && !settings.allow_override,
            };
        }
        used_override = true;
    }
    let gps_validation = if is_mock {
        GpsValidation::mock
    } else if is_outside {
        GpsValidation::outside
    } else {
        GpsValidation::valid
    };
    CheckInRule::Accepted {
        gps_validation,
        used_override,
        has_gps_issue: gps_validation != GpsValidation::valid || is_mock || used_override,
    }
}

/// `dto.overrideReason?.trim() || null` (juga dipakai `note`/`reason`).
pub fn trimmed_or_none(s: Option<&str>) -> Option<String> {
    s.map(trim).filter(|t| !t.is_empty()).map(str::to_owned)
}

/// §5.3.3 — hasil yang wajib dijelaskan (`other`, `rejected`, `moved`).
pub fn note_required(status: StoreVisitStatus) -> bool {
    matches!(
        status,
        StoreVisitStatus::other | StoreVisitStatus::rejected | StoreVisitStatus::moved
    )
}

/// Snapshot GPS check-out (dicatat, tidak pernah memblokir).
pub fn check_out_gps_validation(
    settings: &CompanySettings,
    is_mock: bool,
    accuracy_meters: f64,
    distance_meters: i64,
    radius_meters: i32,
) -> GpsValidation {
    if is_mock {
        GpsValidation::mock
    } else if accuracy_meters > f64::from(settings.max_accuracy_meters) {
        GpsValidation::lowAccuracy
    } else if distance_meters > i64::from(radius_meters) {
        GpsValidation::outside
    } else {
        GpsValidation::valid
    }
}

/// §5.3.5 `Math.max(0, Math.round((out - in) / 1000))`.
pub fn duration_seconds(check_in_ms: i64, check_out_ms: i64) -> i64 {
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let rounded = ((check_out_ms - check_in_ms) as f64 / 1000.0 + 0.5).floor() as i64;
    rounded.max(0)
}

/// Durasi minimum (hanya `success`): `Some((min, duration, remaining))` bila dilanggar.
pub fn min_duration_violation(
    settings: &CompanySettings,
    status: StoreVisitStatus,
    duration: i64,
) -> Option<(i32, i64, i64)> {
    let min_secs = if status == StoreVisitStatus::success {
        i64::from(settings.min_duration_minutes) * 60
    } else {
        0
    };
    (min_secs > 0 && duration < min_secs).then_some((
        settings.min_duration_minutes,
        duration,
        min_secs - duration,
    ))
}

/// Klasifikasi pelanggaran unique (P2002) saat INSERT `storeVisit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UniqueTarget {
    /// `(userId, clientRequestId)` → replay 200.
    Replay,
    /// `storeVisit_one_active_per_user` / `userId` → 409 `VISIT_ALREADY_ACTIVE`.
    AlreadyActive,
    /// Lainnya → dilempar ulang (500).
    Other,
}

/// `uniqueTargets(e)` → keputusan (nama constraint Postgres memuat nama kolom/indeksnya).
pub fn classify_unique(constraint: &str) -> UniqueTarget {
    if constraint.contains("clientRequestId") {
        UniqueTarget::Replay
    } else if constraint.contains("one_active_per_user") || constraint.contains("userId") {
        UniqueTarget::AlreadyActive
    } else {
        UniqueTarget::Other
    }
}

/// `visitNumber`: `VST-` + nomor berpadding 4.
pub fn format_visit_number(n: i32) -> String {
    format!("VST-{n:04}")
}

/// Rekap harian murni: `(visitedStores, totalDurationSeconds, byStatus)` dari kunjungan
/// `completed` saja.
pub fn tally_summary(
    visits: &[(String, Option<String>, Option<i32>)],
) -> (i64, i64, Map<String, Value>) {
    let mut counts: HashMap<&str, i64> = HashMap::new();
    let (mut visited, mut total) = (0i64, 0i64);
    for (state, status, duration) in visits {
        if state != "completed" {
            continue;
        }
        visited += 1;
        total += i64::from(duration.unwrap_or(0));
        if let Some(s) = status {
            *counts.entry(s.as_str()).or_default() += 1;
        }
    }
    let by_status = StoreVisitStatus::ALL
        .iter()
        .map(|s| {
            (
                s.as_str().to_owned(),
                Value::from(counts.get(s.as_str()).copied().unwrap_or(0)),
            )
        })
        .collect();
    (visited, total, by_status)
}

// ───────────────────────── audit percobaan (§9.8) ─────────────────────────

/// Satu baris `storeVisitAuditLog`.
#[derive(Debug, Clone)]
pub struct VisitAuditEntry<'a> {
    pub event: StoreVisitAuditEvent,
    pub company_id: i32,
    pub store_id: i32,
    pub user_id: i32,
    pub visit_id: Option<i32>,
    pub lat: f64,
    pub lng: f64,
    pub accuracy_meters: f64,
    pub distance_meters: i64,
    pub is_mock: bool,
    pub reason: Option<&'a str>,
    pub client_request_id: Option<&'a str>,
}

/// `audit(entry)`: best-effort — galat hanya di-log.
pub async fn write_visit_audit(db: &PgPool, e: &VisitAuditEntry<'_>) {
    let res = sqlx::query(
        r#"INSERT INTO "storeVisitAuditLog"
             ("companyId", "storeId", "userId", "visitId", event, lat, lng, "accuracyMeters", "distanceMeters",
              "isMock", reason, "clientRequestId")
           VALUES ($1, $2, $3, $4, $5::"StoreVisitAuditEvent", $6, $7, $8, $9, $10, $11, $12::uuid)"#,
    )
    .bind(e.company_id)
    .bind(e.store_id)
    .bind(e.user_id)
    .bind(e.visit_id)
    .bind(e.event.as_str())
    .bind(e.lat)
    .bind(e.lng)
    .bind(e.accuracy_meters)
    .bind(i32::try_from(e.distance_meters).unwrap_or(i32::MAX))
    .bind(e.is_mock)
    .bind(e.reason)
    .bind(e.client_request_id)
    .execute(db)
    .await;
    if let Err(err) = res {
        tracing::error!(
            event = e.event.as_str(),
            company_id = e.company_id,
            store_id = e.store_id,
            user_id = e.user_id,
            "storeVisitAuditLog write failed: {err}"
        );
    }
}

// ───────────────────────── efek samping best-effort ─────────────────────────

async fn maybe_open_attendance_shift(
    db: &PgPool,
    settings: &CompanySettings,
    visit: &AttendanceVisitSource,
) {
    if settings.counts_as_attendance {
        open_shift_for_visit(db, visit, settings.time_zone.as_deref()).await;
    }
}

async fn maybe_log_location_event(db: &PgPool, settings: &CompanySettings, ev: BookendEvent) {
    if settings.counts_as_attendance {
        log_bookend_event(db, &ev).await;
    }
}

async fn after_check_in(db: &PgPool, settings: &CompanySettings, visit: &VisitFull) {
    maybe_open_attendance_shift(db, settings, &visit.attendance_source()).await;
    maybe_log_location_event(
        db,
        settings,
        BookendEvent {
            visit_id: visit.id,
            store_id: visit.store_id,
            user_id: visit.user_id,
            event: "enter",
            lat: visit.check_in_lat,
            lng: visit.check_in_lng,
            accuracy: visit.check_in_accuracy,
            at: visit.check_in_at,
        },
    )
    .await;
}

// ───────────────────────── toko yang bisa dikunjungi ─────────────────────────

/// Kolom toko yang dibaca validate-location/check-in.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct VisitableStore {
    pub id: i32,
    #[sqlx(rename = "isActive")]
    pub is_active: bool,
    pub lat: f64,
    pub lng: f64,
    #[sqlx(rename = "radiusMeters")]
    pub radius_meters: Option<i32>,
    #[sqlx(rename = "groupId")]
    pub group_id: Option<i32>,
}

impl VisitableStore {
    fn scope_store(&self) -> ScopeStore {
        ScopeStore {
            id: self.id,
            group_id: self.group_id,
            lat: self.lat,
            lng: self.lng,
        }
    }
}

/// `findVisitableStore`: 404 `STORE_NOT_FOUND` → 422 `STORE_INACTIVE` → 403 `STORE_OUT_OF_SCOPE`
/// (beraudit). Mengembalikan scope yang sudah diresolusi.
async fn find_visitable_store(
    db: &PgPool,
    user_id: i32,
    company_id: i32,
    store_uuid: &str,
    enforce: bool,
    position: ScopePosition,
    client_request_id: Option<&str>,
) -> ApiResult<(VisitableStore, StoreScope)> {
    let store_fut = async {
        Ok::<_, ApiError>(
            sqlx::query_as::<_, VisitableStore>(
                r#"SELECT id, "isActive", lat, lng, "radiusMeters", "groupId" FROM store
                    WHERE uuid = $1::uuid AND "companyId" = $2 AND "deletedAt" IS NULL LIMIT 1"#,
            )
            .bind(store_uuid)
            .bind(company_id)
            .fetch_optional(db)
            .await?,
        )
    };
    let (store, scope) = tokio::try_join!(
        store_fut,
        resolve_store_scope(db, user_id, company_id, Some(enforce))
    )?;
    let store = store.ok_or_else(|| {
        ApiError::coded(StatusCode::NOT_FOUND, "STORE_NOT_FOUND", "Store not found")
    })?;
    if !store.is_active {
        return Err(unprocessable(
            "STORE_INACTIVE",
            "Store is inactive and cannot be visited",
        ));
    }
    assert_in_scope(
        db,
        ScopeDenial {
            scope: &scope,
            company_id,
            user_id,
            store: store.scope_store(),
            position: Some(position),
            client_request_id,
        },
    )
    .await?;
    Ok((store, scope))
}

// ───────────────────────── validate-location ─────────────────────────

/// §6.3 `validateLocation(userId, dto)` — tidak menulis apa pun (selain audit `outOfScope`).
pub async fn validate_location(
    state: &AppState,
    user_id: i32,
    dto: &ValidateLocation,
) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = mz_core::company::require_company_id(db, user_id).await?;
    let settings = get_company_settings(db, company_id).await?;
    let is_mock = dto.is_mock.unwrap_or(false);
    let (store, _) = find_visitable_store(
        db,
        user_id,
        company_id,
        &dto.store_id,
        settings.enforce_group_scope,
        ScopePosition {
            lat: dto.lat,
            lng: dto.lng,
            accuracy_meters: dto.accuracy_meters,
            is_mock: Some(is_mock),
        },
        None,
    )
    .await?;
    let distance = get_distance(dto.lat, dto.lng, store.lat, store.lng);
    let radius = effective_radius(store.radius_meters, settings.default_radius_meters);
    Ok(validate_location_verdict(
        &settings,
        distance,
        radius,
        dto.accuracy_meters,
        is_mock,
    ))
}

// ───────────────────────── check-in ─────────────────────────

fn check_in_response(
    visit: &VisitFull,
    photos_required: &[&str],
    settings: &CompanySettings,
) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), Value::from(visit.uuid.clone()));
    m.insert(
        "visitNumber".into(),
        Value::from(visit.visit_number.clone()),
    );
    m.insert("state".into(), Value::from(visit.state.clone()));
    m.insert(
        "checkIn".into(),
        json!({
            "at": ms(visit.check_in_at),
            "gpsValidation": visit.check_in_gps_validation,
            "distanceMeters": visit.check_in_distance,
        }),
    );
    m.insert("photosRequired".into(), json!(photos_required));
    insert_window(&mut m, settings, visit.check_in_at);
    m.insert("visitType".into(), visit.visit_type_json());
    m.insert("plan".into(), visit.plan_json());
    Value::Object(m)
}

async fn find_by_client_request(
    db: &PgPool,
    user_id: i32,
    crid: &str,
) -> ApiResult<Option<VisitFull>> {
    let crid = crid.to_owned();
    find_visit_full(db, move |qb| {
        qb.push(r#"v."userId" = "#)
            .push_bind(user_id)
            .push(r#" AND v."clientRequestId" = "#)
            .push_bind(crid)
            .push("::uuid");
    })
    .await
}

/// Galat di dalam transaksi check-in.
enum TxError {
    /// `StoreMovedOutOfScope` — diaudit di luar transaksi.
    MovedOutOfScope,
    Sql(sqlx::Error),
    Api(ApiError),
}

impl From<sqlx::Error> for TxError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sql(e)
    }
}

/// `{ data, replayed }` check-in.
pub struct CheckInOutcome {
    pub data: Value,
    pub replayed: bool,
}

/// §6.4 `checkIn(userId, dto)`. `replayed = true` → 200, selain itu 201.
pub async fn check_in(state: &AppState, user_id: i32, dto: &CheckIn) -> ApiResult<CheckInOutcome> {
    let db = &state.db;
    let company_id = mz_core::company::require_company_id(db, user_id).await?;
    let (settings, existing) = tokio::try_join!(
        get_company_settings(db, company_id),
        find_by_client_request(db, user_id, &dto.client_request_id)
    )?;
    let photos_required: Vec<&str> = if settings.require_photos {
        REQUIRED_PHOTO_KINDS.iter().map(|k| k.as_str()).collect()
    } else {
        Vec::new()
    };

    if let Some(existing) = existing {
        after_check_in(db, &settings, &existing).await;
        return Ok(CheckInOutcome {
            data: check_in_response(&existing, &photos_required, &settings),
            replayed: true,
        });
    }

    let method = dto.method.unwrap_or(StoreVisitMethod::gps);
    if method != StoreVisitMethod::gps {
        return Err(ApiError::bad_request(format!(
            "method {method} is not supported yet, use gps"
        )));
    }

    let is_mock = dto.is_mock.unwrap_or(false);
    let position = ScopePosition {
        lat: dto.lat,
        lng: dto.lng,
        accuracy_meters: dto.accuracy_meters,
        is_mock: Some(is_mock),
    };
    let (store, scope) = find_visitable_store(
        db,
        user_id,
        company_id,
        &dto.store_id,
        settings.enforce_group_scope,
        position,
        Some(&dto.client_request_id),
    )
    .await?;

    let active: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "storeVisit" WHERE "userId" = $1 AND state = 'active'::"StoreVisitState" LIMIT 1"#,
    )
    .bind(user_id)
    .fetch_optional(db)
    .await?;
    if active.is_some() {
        return Err(visit_already_active());
    }

    let wants_override = dto.is_override.unwrap_or(false);
    let override_reason = trimmed_or_none(dto.override_reason.as_deref());
    let distance = get_distance(dto.lat, dto.lng, store.lat, store.lng);
    let radius = effective_radius(store.radius_meters, settings.default_radius_meters);
    let audit_entry =
        |event: StoreVisitAuditEvent, visit_id: Option<i32>, mock: bool| VisitAuditEntry {
            event,
            company_id,
            store_id: store.id,
            user_id,
            visit_id,
            lat: dto.lat,
            lng: dto.lng,
            accuracy_meters: dto.accuracy_meters,
            distance_meters: distance,
            is_mock: mock,
            reason: None,
            client_request_id: Some(&dto.client_request_id),
        };

    let (gps_validation, used_override, has_gps_issue) = match check_in_rules(
        &settings,
        distance,
        radius,
        dto.accuracy_meters,
        is_mock,
        wants_override,
        override_reason.as_deref(),
    ) {
        CheckInRule::AccuracyTooLow => {
            return Err(unprocessable(
                "VISIT_GPS_ACCURACY_TOO_LOW",
                "GPS accuracy is too low to verify your location",
            ));
        }
        CheckInRule::MockBlocked => {
            write_visit_audit(
                db,
                &audit_entry(StoreVisitAuditEvent::mockLocationBlocked, None, true),
            )
            .await;
            return Err(unprocessable(
                "VISIT_MOCK_LOCATION_BLOCKED",
                "Mock location is not allowed",
            ));
        }
        CheckInRule::OutOfRadius {
            override_not_allowed,
        } => {
            write_visit_audit(
                db,
                &VisitAuditEntry {
                    reason: override_reason.as_deref(),
                    ..audit_entry(StoreVisitAuditEvent::outOfRadius, None, is_mock)
                },
            )
            .await;
            if override_not_allowed {
                return Err(unprocessable(
                    "VISIT_OVERRIDE_NOT_ALLOWED",
                    "This company does not allow out-of-radius check-in",
                ));
            }
            return Err(ApiError::body(
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({
                    "code": "VISIT_OUT_OF_RADIUS",
                    "message": "You are outside the allowed area for this store",
                    "details": { "distanceMeters": distance },
                }),
            ));
        }
        CheckInRule::Accepted {
            gps_validation,
            used_override,
            has_gps_issue,
        } => (gps_validation, used_override, has_gps_issue),
    };

    let check_in_at = now_naive();
    let client_at = match dto.client_at {
        Some(ms) => Some(
            JsDate::from_millis(ms)
                .ok_or_else(|| internal("Invalid Date for clientAt"))?
                .0,
        ),
        None => None,
    };

    let tx_result: Result<i32, TxError> = async {
        let mut tx = db.begin().await?;
        let last: Option<i32> = sqlx::query_scalar(
            r#"INSERT INTO "storeVisitCounter" ("companyId", "lastNumber") VALUES ($1, 1)
               ON CONFLICT ("companyId") DO UPDATE SET "lastNumber" = "storeVisitCounter"."lastNumber" + 1
               RETURNING "lastNumber""#,
        )
        .bind(company_id)
        .fetch_optional(&mut *tx)
        .await?;
        let last = last.ok_or_else(|| TxError::Api(internal("storeVisitCounter did not return a number")))?;

        // §5.1 — keputusan wilayah dari baca ulang toko di dalam transaksi; snapshot dari baca yang sama.
        let fresh: Option<Option<i32>> = sqlx::query_scalar(r#"SELECT "groupId" FROM store WHERE id = $1"#)
            .bind(store.id)
            .fetch_optional(&mut *tx)
            .await?;
        let store_group_id = fresh.flatten();
        if !is_in_scope(&scope, store_group_id) {
            return Err(TxError::MovedOutOfScope);
        }

        // Klaim jadwal hari ini (UTC) — dikunci dulu, lalu dicek belum diklaim (§0.7).
        let today = today_plan_date();
        let locked: Option<(i32, Option<i32>)> = sqlx::query_as(
            r#"SELECT p.id, p."visitTypeId" FROM "storeVisitPlan" p
                WHERE p."userId" = $1 AND p."storeId" = $2 AND p."visitDate" = $3::date AND p."deletedAt" IS NULL
                FOR UPDATE"#,
        )
        .bind(user_id)
        .bind(store.id)
        .bind(&today)
        .fetch_optional(&mut *tx)
        .await?;
        let plan = match locked {
            Some((plan_id, vt)) => {
                let claimed: Option<i32> = sqlx::query_scalar(r#"SELECT id FROM "storeVisit" WHERE "planId" = $1"#)
                    .bind(plan_id)
                    .fetch_optional(&mut *tx)
                    .await?;
                claimed.is_none().then_some((plan_id, vt))
            }
            None => None,
        };

        let id: i32 = sqlx::query_scalar(
            r#"INSERT INTO "storeVisit"
                 ("companyId", "storeId", "storeGroupId", "userId", "visitNumber", "clientRequestId", state,
                  "checkInAt", "checkInClientAt", "checkInLat", "checkInLng", "checkInAccuracy", "checkInDistance",
                  "checkInMethod", "checkInGpsValidation", "checkInIsMock", "checkInIsOverride",
                  "checkInOverrideReason", "hasGpsIssue", "planId", "visitTypeId")
               VALUES ($1, $2, $3, $4, $5, $6::uuid, 'active'::"StoreVisitState", $7, $8, $9, $10, $11, $12,
                       $13::"StoreVisitMethod", $14::"GpsValidation", $15, $16, $17, $18, $19, $20)
               RETURNING id"#,
        )
        .bind(company_id)
        .bind(store.id)
        .bind(store_group_id)
        .bind(user_id)
        .bind(format_visit_number(last))
        .bind(&dto.client_request_id)
        .bind(check_in_at)
        .bind(client_at)
        .bind(dto.lat)
        .bind(dto.lng)
        .bind(dto.accuracy_meters)
        .bind(i32::try_from(distance).unwrap_or(i32::MAX))
        .bind(method.as_str())
        .bind(gps_validation.as_str())
        .bind(is_mock)
        .bind(used_override)
        .bind(if used_override { override_reason.as_deref() } else { None })
        .bind(has_gps_issue)
        .bind(plan.map(|p| p.0))
        .bind(plan.and_then(|p| p.1))
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }
    .await;

    match tx_result {
        Ok(visit_id) => {
            if used_override {
                write_visit_audit(
                    db,
                    &VisitAuditEntry {
                        reason: override_reason.as_deref(),
                        ..audit_entry(StoreVisitAuditEvent::overrideUsed, Some(visit_id), is_mock)
                    },
                )
                .await;
            }
            let visit = find_visit_full(db, |qb| {
                qb.push("v.id = ").push_bind(visit_id);
            })
            .await?
            .ok_or_else(|| internal("created visit vanished"))?;
            after_check_in(db, &settings, &visit).await;
            Ok(CheckInOutcome {
                data: check_in_response(&visit, &photos_required, &settings),
                replayed: false,
            })
        }
        Err(TxError::MovedOutOfScope) => Err(deny_out_of_scope(
            db,
            ScopeDenial {
                scope: &scope,
                company_id,
                user_id,
                store: store.scope_store(),
                position: Some(position),
                client_request_id: Some(&dto.client_request_id),
            },
        )
        .await),
        Err(TxError::Api(e)) => Err(e),
        Err(TxError::Sql(e)) => {
            if !mz_core::db::is_unique_violation(&e) {
                return Err(e.into());
            }
            let constraint = e
                .as_database_error()
                .and_then(|d| d.constraint())
                .unwrap_or_default()
                .to_owned();
            match classify_unique(&constraint) {
                UniqueTarget::Replay => {
                    match find_by_client_request(db, user_id, &dto.client_request_id).await? {
                        Some(replay) => {
                            after_check_in(db, &settings, &replay).await;
                            Ok(CheckInOutcome {
                                data: check_in_response(&replay, &photos_required, &settings),
                                replayed: true,
                            })
                        }
                        None => Err(e.into()),
                    }
                }
                UniqueTarget::AlreadyActive => Err(visit_already_active()),
                UniqueTarget::Other => Err(e.into()),
            }
        }
    }
}

// ───────────────────────── kunjungan aktif milik caller ─────────────────────────

/// `findActiveOwnVisit(userId, visitId)`: toko di-join TANPA `deletedAt` (§8.2).
async fn find_active_own_visit(
    db: &PgPool,
    user_id: i32,
    visit_uuid: &str,
) -> ApiResult<VisitFull> {
    let uuid = visit_uuid.to_owned();
    let visit = find_visit_full(db, move |qb| {
        qb.push("v.uuid = ")
            .push_bind(uuid)
            .push(r#"::uuid AND v."userId" = "#)
            .push_bind(user_id);
    })
    .await?
    .ok_or_else(visit_not_found)?;
    if visit.state != "active" {
        return Err(visit_not_active());
    }
    Ok(visit)
}

// ───────────────────────── saran toko berikutnya ─────────────────────────

/// Saran `nextStore` `{ id, name, distanceMeters }`.
#[derive(Debug, Clone, PartialEq)]
pub struct NextStore {
    pub id: String,
    pub name: String,
    pub distance_meters: i64,
}

impl NextStore {
    fn to_json(&self) -> Value {
        json!({ "id": self.id, "name": self.name, "distanceMeters": self.distance_meters })
    }
}

/// Pilih kandidat terdekat dalam radius pencarian (urutan kandidat dipertahankan untuk seri).
pub fn nearest_candidate(
    lat: f64,
    lng: f64,
    candidates: &[(String, String, f64, f64)],
) -> Option<NextStore> {
    #[allow(clippy::cast_possible_truncation)]
    let radius = (DEFAULT_RADIUS_KM * 1000.0) as i64;
    let mut best: Option<NextStore> = None;
    for (uuid, name, slat, slng) in candidates
        .iter()
        .take(usize::try_from(MAX_PREFILTER_ROWS).unwrap_or(300))
    {
        let d = get_distance(lat, lng, *slat, *slng);
        if d > radius {
            continue;
        }
        if best.as_ref().is_none_or(|b| d < b.distance_meters) {
            best = Some(NextStore {
                id: uuid.clone(),
                name: name.clone(),
                distance_meters: d,
            });
        }
    }
    best
}

async fn find_next_store_inner(
    db: &PgPool,
    company_id: i32,
    user_id: i32,
    lat: f64,
    lng: f64,
    zone: &str,
) -> ApiResult<Option<NextStore>> {
    let (day_start, _) = local_day_bounds(zone, local_date(zone, now_naive()));
    let visited: Vec<i32> = sqlx::query_scalar(
        r#"SELECT DISTINCT "storeId" FROM "storeVisit" WHERE "companyId" = $1 AND "userId" = $2 AND "checkInAt" >= $3"#,
    )
    .bind(company_id)
    .bind(user_id)
    .bind(day_start)
    .fetch_all(db)
    .await?;
    let scope = resolve_store_scope(db, user_id, company_id, None).await?;
    let bx = bounding_box(lat, lng, DEFAULT_RADIUS_KM);
    let today = parse_plan_date(&today_plan_date()).ok_or_else(|| internal("bad today"))?;
    let scheduled: Vec<i32> = sqlx::query_scalar(
        r#"SELECT "storeId" FROM "storeVisitPlan"
            WHERE "companyId" = $1 AND "userId" = $2 AND "deletedAt" IS NULL AND "visitDate" = $3"#,
    )
    .bind(company_id)
    .bind(user_id)
    .bind(today)
    .fetch_all(db)
    .await?;
    let scheduled: Vec<i32> = scheduled
        .into_iter()
        .filter(|id| !visited.contains(id))
        .collect();

    let query = |ids_in: Option<&Vec<i32>>, ids_not_in: Option<&Vec<i32>>| {
        let mut qb = QueryBuilder::<Postgres>::new(
            r#"SELECT s.uuid::text, s.name, s.lat, s.lng FROM store s WHERE s."companyId" = "#,
        );
        qb.push_bind(company_id)
            .push(r#" AND s."deletedAt" IS NULL AND s."isActive" = true AND s.lat >= "#)
            .push_bind(bx.lat.gte)
            .push(" AND s.lat <= ")
            .push_bind(bx.lat.lte)
            .push(" AND s.lng >= ")
            .push_bind(bx.lng.gte)
            .push(" AND s.lng <= ")
            .push_bind(bx.lng.lte);
        if let Some(ids) = ids_in {
            qb.push(" AND s.id = ANY(").push_bind(ids.clone()).push(")");
        }
        if let Some(ids) = ids_not_in.filter(|v| !v.is_empty()) {
            qb.push(" AND NOT (s.id = ANY(")
                .push_bind(ids.clone())
                .push("))");
        }
        push_scope_clause(&mut qb, &scope, r#"s."groupId""#);
        qb.push(" ORDER BY s.id LIMIT ")
            .push_bind(MAX_PREFILTER_ROWS + 1);
        qb
    };
    let mut candidates: Vec<(String, String, f64, f64)> = if scheduled.is_empty() {
        Vec::new()
    } else {
        query(Some(&scheduled), None)
            .build_query_as()
            .fetch_all(db)
            .await?
    };
    if candidates.is_empty() {
        candidates = query(None, Some(&visited))
            .build_query_as()
            .fetch_all(db)
            .await?;
    }
    if i64::try_from(candidates.len()).unwrap_or(i64::MAX) > MAX_PREFILTER_ROWS {
        tracing::warn!(
            "nextStore for company {company_id} scanned a truncated candidate set: more than {MAX_PREFILTER_ROWS} stores within {DEFAULT_RADIUS_KM} km"
        );
    }
    Ok(nearest_candidate(lat, lng, &candidates))
}

/// `findNextStore`: toko aktif terdekat yang belum dikunjungi hari ini (jadwal hari ini didahulukan).
/// Galat apa pun → `None`.
pub async fn find_next_store(
    db: &PgPool,
    company_id: i32,
    user_id: i32,
    lat: f64,
    lng: f64,
    zone: &str,
) -> Option<NextStore> {
    match find_next_store_inner(db, company_id, user_id, lat, lng, zone).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("failed to compute nextStore for user {user_id}: {e:?}");
            None
        }
    }
}

// ───────────────────────── check-out ─────────────────────────

/// §6.7 `checkOut(userId, visitId, dto)`.
pub async fn check_out(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    visit_uuid: &str,
    dto: &CheckOut,
) -> ApiResult<Value> {
    let db = &state.db;
    let visit = find_active_own_visit(db, user_id, visit_uuid).await?;
    let note = trimmed_or_none(dto.note.as_deref());
    if note_required(dto.status) && note.is_none() {
        return Err(unprocessable(
            "VISIT_NOTE_REQUIRED",
            format!("A note is required when the visit result is {}", dto.status),
        ));
    }
    let (settings, photo_status) = tokio::try_join!(
        get_company_settings(db, visit.company_id),
        confirmed_photo_status(db, visit.id)
    )?;
    if settings.require_photos && !photo_status.missing_required_kinds.is_empty() {
        return Err(ApiError::body(
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "code": "VISIT_PHOTO_REQUIRED",
                "message": "Upload the required photos before checking out",
                "details": { "missingPhotoKinds": photo_status.missing_required_kinds },
            }),
        ));
    }
    let is_mock = dto.is_mock.unwrap_or(false);
    let distance = get_distance(dto.lat, dto.lng, visit.store_lat, visit.store_lng);
    let radius = effective_radius(visit.store_radius_meters, settings.default_radius_meters);
    let gps_validation =
        check_out_gps_validation(&settings, is_mock, dto.accuracy_meters, distance, radius);
    let check_out_at = now_naive();
    let duration = duration_seconds(ms(visit.check_in_at), ms(check_out_at));
    if let Some((min, dur, remaining)) = min_duration_violation(&settings, dto.status, duration) {
        return Err(ApiError::body(
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({
                "code": "VISIT_DURATION_TOO_SHORT",
                "message": format!("Stay at the store for at least {min} minutes before checking out"),
                "details": { "minDurationMinutes": min, "durationSeconds": dur, "remainingSeconds": remaining },
            }),
        ));
    }
    let client_at = match dto.client_at {
        Some(ms) => Some(
            JsDate::from_millis(ms)
                .ok_or_else(|| internal("Invalid Date for clientAt"))?
                .0,
        ),
        None => None,
    };
    let tags = match &dto.tags {
        mz_sales_geo::dto::Field::Null => {
            return Err(internal("Argument `tags` must not be null"));
        }
        mz_sales_geo::dto::Field::Val(t) => Some(t.clone()),
        mz_sales_geo::dto::Field::Absent => None,
    };

    let pre = audit
        .pre_read(Model::Store, Lookup::Id(visit.store_id))
        .await;
    let mut tx = db.begin().await?;
    let n = sqlx::query(
        r#"UPDATE "storeVisit" SET state = 'completed'::"StoreVisitState", status = $2::"StoreVisitStatus",
                  note = $3, tags = COALESCE($4, tags), "durationSeconds" = $5, "checkOutAt" = $6,
                  "checkOutClientAt" = $7, "checkOutLat" = $8, "checkOutLng" = $9, "checkOutAccuracy" = $10,
                  "checkOutDistance" = $11, "checkOutGpsValidation" = $12::"GpsValidation", "checkOutIsMock" = $13
            WHERE id = $1 AND state = 'active'::"StoreVisitState""#,
    )
    .bind(visit.id)
    .bind(dto.status.as_str())
    .bind(note.as_deref())
    .bind(tags)
    .bind(i32::try_from(duration).unwrap_or(i32::MAX))
    .bind(check_out_at)
    .bind(client_at)
    .bind(dto.lat)
    .bind(dto.lng)
    .bind(dto.accuracy_meters)
    .bind(i32::try_from(distance).unwrap_or(i32::MAX))
    .bind(gps_validation.as_str())
    .bind(is_mock)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        drop(tx);
        return Err(visit_not_active());
    }
    sqlx::query(
        r#"UPDATE store SET "totalVisits" = "totalVisits" + 1, "lastVisitAt" = $2,
                  "lastVisitStatus" = $3::"StoreVisitStatus"
            WHERE id = $1"#,
    )
    .bind(visit.store_id)
    .bind(check_out_at)
    .bind(dto.status.as_str())
    .execute(&mut *tx)
    .await?;
    if let Some(after) = audit
        .result_row(&mut *tx, Model::Store, Lookup::Id(visit.store_id))
        .await
    {
        audit.record_update(Model::Store, pre, &after).await;
    }
    tx.commit().await?;

    maybe_log_location_event(
        db,
        &settings,
        BookendEvent {
            visit_id: visit.id,
            store_id: visit.store_id,
            user_id,
            event: "exit",
            lat: dto.lat,
            lng: dto.lng,
            accuracy: dto.accuracy_meters,
            at: check_out_at,
        },
    )
    .await;
    let next = find_next_store(
        db,
        visit.company_id,
        user_id,
        dto.lat,
        dto.lng,
        settings.zone(),
    )
    .await;
    Ok(json!({
        "id": visit.uuid,
        "state": "completed",
        "status": dto.status,
        "checkOut": { "at": ms(check_out_at), "gpsValidation": gps_validation, "distanceMeters": distance },
        "durationSeconds": duration,
        "photosCount": photo_status.photos_count,
        "nextStore": next.as_ref().map_or(Value::Null, NextStore::to_json),
    }))
}

// ───────────────────────── cancel ─────────────────────────

/// §6.11 `cancel(userId, visitId, dto)`: catatan → `note`, klaim jadwal dilepas (`planId = null`).
pub async fn cancel(
    db: &PgPool,
    user_id: i32,
    visit_uuid: &str,
    reason: Option<&str>,
) -> ApiResult<Value> {
    let visit = find_active_own_visit(db, user_id, visit_uuid).await?;
    let reason = trimmed_or_none(reason);
    let n = sqlx::query(
        r#"UPDATE "storeVisit" SET state = 'cancelled'::"StoreVisitState", note = COALESCE($2, note), "planId" = NULL
            WHERE id = $1 AND state = 'active'::"StoreVisitState""#,
    )
    .bind(visit.id)
    .bind(reason.as_deref())
    .execute(db)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(visit_not_active());
    }
    Ok(json!({ "id": visit.uuid, "state": "cancelled" }))
}

// ───────────────────────── baca ─────────────────────────

/// §6.6 `getActiveVisit(userId)` → objek atau `null`.
pub async fn get_active_visit(db: &PgPool, user_id: i32) -> ApiResult<Value> {
    let company_id = mz_core::company::require_company_id(db, user_id).await?;
    let Some(visit) = find_visit_full(db, |qb| {
        qb.push(r#"v."userId" = "#)
            .push_bind(user_id)
            .push(r#" AND v."companyId" = "#)
            .push_bind(company_id)
            .push(r#" AND v.state = 'active'::"StoreVisitState""#);
    })
    .await?
    else {
        return Ok(Value::Null);
    };
    let (photo_status, settings) = tokio::try_join!(
        confirmed_photo_status(db, visit.id),
        get_company_settings(db, company_id)
    )?;
    let mut m = Map::new();
    m.insert("id".into(), Value::from(visit.uuid.clone()));
    m.insert(
        "visitNumber".into(),
        Value::from(visit.visit_number.clone()),
    );
    m.insert("store".into(), visit.store_ref());
    m.insert("checkIn".into(), json!({ "at": ms(visit.check_in_at) }));
    m.insert("photosCount".into(), Value::from(photo_status.photos_count));
    m.insert("photoKinds".into(), json!(photo_status.photo_kinds));
    m.insert("state".into(), Value::from(visit.state.clone()));
    m.insert("visitType".into(), visit.visit_type_json());
    insert_window(&mut m, &settings, visit.check_in_at);
    Ok(Value::Object(m))
}

/// §6.8 `findVisitsForUser(userId, query)` → `{ data, meta }`.
pub async fn find_visits_for_user(
    db: &PgPool,
    user_id: i32,
    q: &ListVisitsQuery,
) -> ApiResult<Value> {
    let company_id = mz_core::company::require_company_id(db, user_id).await?;
    let mut gte = q
        .start_date
        .as_deref()
        .map(|d| day_edge(d, "T00:00:00.000Z"))
        .transpose()?;
    let lte = q
        .end_date
        .as_deref()
        .map(|d| day_edge(d, "T23:59:59.999Z"))
        .transpose()?;
    let history_days = mz_sales_geo::plan::resolve_visit_history_days(db, company_id).await?;
    if history_days > 0 {
        let floor = Utc::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap_or_default()
            - Duration::days(i64::from(history_days) - 1);
        if gte.is_none_or(|g| g < floor) {
            gte = Some(floor);
        }
    }

    let push_where = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(r#" WHERE v."userId" = "#)
            .push_bind(user_id)
            .push(r#" AND v."companyId" = "#)
            .push_bind(company_id);
        if let Some(sid) = &q.store_id {
            qb.push(" AND s.uuid = ")
                .push_bind(sid.clone())
                .push("::uuid");
        }
        if let Some(st) = q.status.as_ref().filter(|s| !s.is_empty()) {
            let v: Vec<String> = st.iter().map(|s| s.as_str().to_owned()).collect();
            qb.push(" AND v.status::text = ANY(").push_bind(v).push(")");
        }
        if let Some(g) = gte {
            qb.push(r#" AND v."checkInAt" >= "#).push_bind(g);
        }
        if let Some(l) = lte {
            qb.push(r#" AND v."checkInAt" <= "#).push_bind(l);
        }
        if let Some(s) = &q.search {
            // Prisma `contains` + `insensitive` = ILIKE tanpa escape `%`/`_`.
            qb.push(" AND (v.note ILIKE '%' || ")
                .push_bind(s.clone())
                .push(" || '%' OR s.name ILIKE '%' || ")
                .push_bind(s.clone())
                .push(" || '%' OR s.code ILIKE '%' || ")
                .push_bind(s.clone())
                .push(" || '%')");
        }
    };

    let mut qb = QueryBuilder::<Postgres>::new(VISIT_FULL_SELECT);
    push_where(&mut qb);
    qb.push(r#" ORDER BY v."checkInAt" DESC, v.id DESC LIMIT "#)
        .push_bind(q.limit)
        .push(" OFFSET ")
        .push_bind((q.page - 1) * q.limit);
    let visits: Vec<VisitFull> = qb.build_query_as().fetch_all(db).await?;

    let mut cq = QueryBuilder::<Postgres>::new(
        r#"SELECT COUNT(*) FROM "storeVisit" v JOIN store s ON s.id = v."storeId""#,
    );
    push_where(&mut cq);
    let total: i64 = cq.build_query_scalar().fetch_one(db).await?;

    let ids: Vec<i32> = visits.iter().map(|v| v.id).collect();
    let counts = confirmed_photo_counts(db, &ids).await?;
    let data: Vec<Value> = visits
        .iter()
        .map(|v| {
            json!({
                "id": v.uuid,
                "visitNumber": v.visit_number,
                "store": v.store_ref(),
                "checkInAt": ms(v.check_in_at),
                "checkOutAt": opt_ms(v.check_out_at),
                "durationSeconds": v.duration_seconds,
                "status": v.status,
                "visitType": v.visit_type_json(),
                "photosCount": counts.get(&v.id).copied().unwrap_or(0),
                "note": v.note,
            })
        })
        .collect();
    Ok(json!({
        "data": data,
        "meta": { "total": total, "page": q.page, "limit": q.limit, "totalPages": total_pages(total, q.limit) },
    }))
}

/// §6.9 `findVisitForUser(userId, visitId)` — dicakup `userId`, tanpa `sales`/`isMock`.
pub async fn find_visit_for_user(
    state: &AppState,
    user_id: i32,
    visit_uuid: &str,
) -> ApiResult<Value> {
    let db = &state.db;
    let uuid = visit_uuid.to_owned();
    let visit = find_visit_full(db, move |qb| {
        qb.push("v.uuid = ")
            .push_bind(uuid)
            .push(r#"::uuid AND v."userId" = "#)
            .push_bind(user_id);
    })
    .await?
    .ok_or_else(visit_not_found)?;
    let photo_rows = photos_of_visits(db, &[visit.id]).await?;
    let photos = serialize_photos(state, &visit.uuid, &photo_rows)?;
    let check_out = match visit.check_out_at {
        Some(at) => json!({
            "at": ms(at),
            "clientAt": opt_ms(visit.check_out_client_at),
            "lat": opt_num(visit.check_out_lat),
            "lng": opt_num(visit.check_out_lng),
            "accuracyMeters": opt_num(visit.check_out_accuracy),
            "distanceMeters": visit.check_out_distance,
            "gpsValidation": visit.check_out_gps_validation,
        }),
        None => Value::Null,
    };
    Ok(json!({
        "id": visit.uuid,
        "visitNumber": visit.visit_number,
        "store": {
            "id": visit.store_uuid,
            "code": visit.store_code,
            "name": visit.store_name,
            "ownerName": visit.store_owner_name,
        },
        "state": visit.state,
        "status": visit.status,
        "visitType": visit.visit_type_json(),
        "checkIn": {
            "at": ms(visit.check_in_at),
            "clientAt": opt_ms(visit.check_in_client_at),
            "lat": num(visit.check_in_lat),
            "lng": num(visit.check_in_lng),
            "accuracyMeters": num(visit.check_in_accuracy),
            "distanceMeters": visit.check_in_distance,
            "method": visit.check_in_method,
            "gpsValidation": visit.check_in_gps_validation,
            "isOverride": visit.check_in_is_override,
            "overrideReason": visit.check_in_override_reason,
        },
        "checkOut": check_out,
        "durationSeconds": visit.duration_seconds,
        "note": visit.note,
        "tags": visit.tags,
        "photos": photos,
        "createdAt": ms(visit.created_at),
    }))
}

/// §6.10 `getSummary(userId, query)`. Tanggal non-`YYYY-MM-DD` → 500 (Invalid Date di Node).
pub async fn get_summary(db: &PgPool, user_id: i32, date: &str) -> ApiResult<Value> {
    let company_id = mz_core::company::require_company_id(db, user_id).await?;
    let settings = get_company_settings(db, company_id).await?;
    let day = parse_plan_date(date).ok_or_else(|| internal(format!("Invalid Date: {date}")))?;
    let zone = settings.zone().to_owned();
    let (day_start, day_end) = local_day_bounds(&zone, day);

    #[derive(sqlx::FromRow)]
    struct Row {
        state: String,
        status: Option<String>,
        #[sqlx(rename = "durationSeconds")]
        duration: Option<i32>,
        #[sqlx(rename = "checkInLat")]
        in_lat: f64,
        #[sqlx(rename = "checkInLng")]
        in_lng: f64,
        #[sqlx(rename = "checkOutLat")]
        out_lat: Option<f64>,
        #[sqlx(rename = "checkOutLng")]
        out_lng: Option<f64>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        r#"SELECT state::text AS state, status::text AS status, "durationSeconds", "checkInLat", "checkInLng",
                  "checkOutLat", "checkOutLng"
             FROM "storeVisit"
            WHERE "userId" = $1 AND "companyId" = $2 AND "checkInAt" >= $3 AND "checkInAt" <= $4
            ORDER BY "checkInAt" DESC, id DESC"#,
    )
    .bind(user_id)
    .bind(company_id)
    .bind(day_start)
    .bind(day_end)
    .fetch_all(db)
    .await?;
    let tally_in: Vec<(String, Option<String>, Option<i32>)> = rows
        .iter()
        .map(|r| (r.state.clone(), r.status.clone(), r.duration))
        .collect();
    let (visited, total_duration, by_status) = tally_summary(&tally_in);

    let next = match rows.first() {
        Some(r) => {
            find_next_store(
                db,
                company_id,
                user_id,
                r.out_lat.unwrap_or(r.in_lat),
                r.out_lng.unwrap_or(r.in_lng),
                &zone,
            )
            .await
        }
        None => None,
    };
    let (planned, fulfilled): (i64, i64) = sqlx::query_as(
        r#"SELECT COUNT(*),
                  COUNT(*) FILTER (WHERE EXISTS (SELECT 1 FROM "storeVisit" v
                                                  WHERE v."planId" = p.id AND v.state <> 'cancelled'::"StoreVisitState"))
             FROM "storeVisitPlan" p
            WHERE p."companyId" = $1 AND p."userId" = $2 AND p."deletedAt" IS NULL AND p."visitDate" = $3"#,
    )
    .bind(company_id)
    .bind(user_id)
    .bind(day)
    .fetch_one(db)
    .await?;
    Ok(json!({
        "date": date,
        "visitedStores": visited,
        "totalDurationSeconds": total_duration,
        "byStatus": by_status,
        "nextStore": next.as_ref().map_or(Value::Null, NextStore::to_json),
        "plannedStores": planned,
        "fulfilledStores": fulfilled,
        "remainingPlannedStores": planned - fulfilled,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> CompanySettings {
        CompanySettings {
            default_radius_meters: 150,
            max_accuracy_meters: 100,
            block_mock_location: true,
            require_photos: true,
            allow_override: false,
            enforce_group_scope: false,
            min_duration_minutes: 0,
            counts_as_attendance: false,
            time_zone: Some("Asia/Jakarta".into()),
        }
    }

    // ── check-in (store-visits.service.spec.ts:284) ──

    #[test]
    fn accepts_inside_radius() {
        assert_eq!(
            check_in_rules(&settings(), 20, 150, 10.0, false, false, None),
            CheckInRule::Accepted {
                gps_validation: GpsValidation::valid,
                used_override: false,
                has_gps_issue: false
            }
        );
    }

    #[test]
    fn accuracy_threshold() {
        assert_eq!(
            check_in_rules(&settings(), 0, 150, 100.1, false, false, None),
            CheckInRule::AccuracyTooLow
        );
        // "accepts a check-in exactly at the accuracy threshold"
        assert!(matches!(
            check_in_rules(&settings(), 0, 150, 100.0, false, false, None),
            CheckInRule::Accepted { .. }
        ));
        // "reports low accuracy before anything else" (mock + outside juga)
        assert_eq!(
            check_in_rules(&settings(), 999, 150, 500.0, true, true, Some("x")),
            CheckInRule::AccuracyTooLow
        );
    }

    #[test]
    fn mock_rules() {
        assert_eq!(
            check_in_rules(&settings(), 0, 150, 5.0, true, false, None),
            CheckInRule::MockBlocked
        );
        let lenient = CompanySettings {
            block_mock_location: false,
            ..settings()
        };
        // "stores a mock location as a gps issue when the company allows it"
        assert_eq!(
            check_in_rules(&lenient, 0, 150, 5.0, true, false, None),
            CheckInRule::Accepted {
                gps_validation: GpsValidation::mock,
                used_override: false,
                has_gps_issue: true
            }
        );
    }

    #[test]
    fn radius_and_override() {
        let s = settings();
        // radius boundary is inclusive
        assert!(matches!(
            check_in_rules(&s, 150, 150, 5.0, false, false, None),
            CheckInRule::Accepted { .. }
        ));
        assert_eq!(
            check_in_rules(&s, 151, 150, 5.0, false, false, None),
            CheckInRule::OutOfRadius {
                override_not_allowed: false
            }
        );
        // "rejects an override request when the company forbids overrides"
        assert_eq!(
            check_in_rules(&s, 151, 150, 5.0, false, true, Some("macet")),
            CheckInRule::OutOfRadius {
                override_not_allowed: true
            }
        );
        let allow = CompanySettings {
            allow_override: true,
            ..settings()
        };
        // "rejects an override with a blank reason even when the company allows overrides"
        assert_eq!(
            check_in_rules(
                &allow,
                151,
                150,
                5.0,
                false,
                true,
                trimmed_or_none(Some("   ")).as_deref()
            ),
            CheckInRule::OutOfRadius {
                override_not_allowed: false
            }
        );
        // "accepts a complete override"
        assert_eq!(
            check_in_rules(&allow, 500, 150, 5.0, false, true, Some("macet")),
            CheckInRule::Accepted {
                gps_validation: GpsValidation::outside,
                used_override: true,
                has_gps_issue: true
            }
        );
        // "ignores isOverride when the salesperson is already inside the radius"
        assert_eq!(
            check_in_rules(&allow, 10, 150, 5.0, false, true, Some("macet")),
            CheckInRule::Accepted {
                gps_validation: GpsValidation::valid,
                used_override: false,
                has_gps_issue: false
            }
        );
    }

    #[test]
    fn store_radius_wins() {
        assert_eq!(
            effective_radius(Some(50), settings().default_radius_meters),
            50
        );
        assert_eq!(
            effective_radius(None, settings().default_radius_meters),
            150
        );
    }

    #[test]
    fn check_out_window_from_stored_check_in() {
        let at = JsDate::from_millis(1_000_000).unwrap().0;
        assert_eq!(check_out_window(&settings(), at), (0, None));
        let s = CompanySettings {
            min_duration_minutes: 5,
            ..settings()
        };
        assert_eq!(check_out_window(&s, at), (5, Some(1_000_000 + 300_000)));
    }

    #[test]
    fn unique_classification() {
        assert_eq!(
            classify_unique("storeVisit_userId_clientRequestId_key"),
            UniqueTarget::Replay
        );
        assert_eq!(
            classify_unique("storeVisit_one_active_per_user"),
            UniqueTarget::AlreadyActive
        );
        assert_eq!(
            classify_unique("storeVisit_planId_key"),
            UniqueTarget::Other
        );
        assert_eq!(
            classify_unique("storeVisit_companyId_visitNumber_key"),
            UniqueTarget::Other
        );
        assert_eq!(format_visit_number(7), "VST-0007");
        assert_eq!(format_visit_number(12345), "VST-12345");
    }

    // ── validateLocation (spec:1107) ──

    #[test]
    fn validate_location_verdicts() {
        let s = settings();
        let v = validate_location_verdict(&s, 20, 150, 5.0, false);
        assert_eq!(
            v,
            json!({"gpsValidation":"valid","distanceMeters":20,"canCheckIn":true,"canRequestOverride":false,"message":"Location verified"})
        );
        let v = validate_location_verdict(&s, 400, 150, 5.0, false);
        assert_eq!(v["gpsValidation"], "outside");
        assert_eq!(
            v["message"],
            "You are 400 m from the store, outside the allowed area"
        );
        assert!(!v.to_string().contains("150"));
        assert_eq!(v["canRequestOverride"], false);
        let allow = CompanySettings {
            allow_override: true,
            ..settings()
        };
        assert_eq!(
            validate_location_verdict(&allow, 400, 150, 5.0, false)["canRequestOverride"],
            true
        );
        assert_eq!(
            validate_location_verdict(&s, 400, 150, 101.0, true)["gpsValidation"],
            "lowAccuracy"
        );
        assert_eq!(
            validate_location_verdict(&s, 0, 150, 5.0, true)["message"],
            "Mock location is not allowed"
        );
        let lenient = CompanySettings {
            block_mock_location: false,
            ..settings()
        };
        let v = validate_location_verdict(&lenient, 0, 150, 5.0, true);
        assert_eq!(
            (v["gpsValidation"].as_str(), v["canCheckIn"].as_bool()),
            (Some("mock"), Some(true))
        );
    }

    // ── checkOut (spec:1297) ──

    #[test]
    fn check_out_rules() {
        let s = settings();
        assert!(note_required(StoreVisitStatus::other));
        assert!(note_required(StoreVisitStatus::moved));
        assert!(note_required(StoreVisitStatus::rejected));
        assert!(!note_required(StoreVisitStatus::success));
        assert_eq!(trimmed_or_none(Some("  \t ")), None);
        assert_eq!(
            check_out_gps_validation(&s, true, 500.0, 999, 150),
            GpsValidation::mock
        );
        assert_eq!(
            check_out_gps_validation(&s, false, 500.0, 999, 150),
            GpsValidation::lowAccuracy
        );
        assert_eq!(
            check_out_gps_validation(&s, false, 5.0, 999, 150),
            GpsValidation::outside
        );
        assert_eq!(
            check_out_gps_validation(&s, false, 5.0, 10, 150),
            GpsValidation::valid
        );
        assert_eq!(duration_seconds(0, 1_499), 1);
        assert_eq!(duration_seconds(0, 1_500), 2);
        assert_eq!(duration_seconds(5_000, 0), 0);
    }

    #[test]
    fn min_duration() {
        let s = CompanySettings {
            min_duration_minutes: 10,
            ..settings()
        };
        assert_eq!(
            min_duration_violation(&s, StoreVisitStatus::success, 300),
            Some((10, 300, 300))
        );
        // "accepts a check-out that exactly meets the minimum duration"
        assert_eq!(
            min_duration_violation(&s, StoreVisitStatus::success, 600),
            None
        );
        // other results close immediately
        assert_eq!(
            min_duration_violation(&s, StoreVisitStatus::closed, 5),
            None
        );
        // "never applies a minimum duration when the company leaves it at 0"
        assert_eq!(
            min_duration_violation(&settings(), StoreVisitStatus::success, 0),
            None
        );
    }

    // ── nextStore (spec:1779) ──

    #[test]
    fn nearest() {
        let c = vec![
            ("far".to_owned(), "Far".to_owned(), -7.80, 110.30),
            ("near".to_owned(), "Near".to_owned(), -7.7502, 110.2974),
            ("out".to_owned(), "Out".to_owned(), -6.0, 106.0),
        ];
        let n = nearest_candidate(-7.7501, 110.2973, &c).unwrap();
        assert_eq!(n.id, "near");
        assert_eq!(nearest_candidate(-7.75, 110.29, &c[2..]), None);
        assert_eq!(nearest_candidate(0.0, 0.0, &[]), None);
    }

    // ── getSummary (spec:2529) ──

    #[test]
    fn summary_tally() {
        let (v, t, by) = tally_summary(&[]);
        assert_eq!((v, t), (0, 0));
        assert_eq!(
            Value::Object(by),
            json!({"success":0,"closed":0,"moved":0,"noOwner":0,"rejected":0,"other":0})
        );
        let rows = vec![
            (
                "completed".to_owned(),
                Some("success".to_owned()),
                Some(100),
            ),
            ("completed".to_owned(), Some("closed".to_owned()), None),
            ("cancelled".to_owned(), None, None),
            ("active".to_owned(), None, None),
        ];
        let (v, t, by) = tally_summary(&rows);
        assert_eq!((v, t), (2, 100));
        assert_eq!(by["success"], 1);
        assert_eq!(by["closed"], 1);
    }
}
