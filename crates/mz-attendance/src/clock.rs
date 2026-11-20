//! Port `MobileClockService` bagian shift & istirahat (Node:
//! `src/mobile-clock/mobile-clock.service.ts:78-800`) + `previous-day-shift.ts`.

use axum::http::StatusCode;
use chrono::{NaiveDateTime, TimeZone};
use mz_core::time::{JsDate, now_naive};
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};

use crate::body::{BreakStartDto, ClockInDto, ClockOutDto, GpsStatusDto, Photo};
use crate::geo;

/// `DEFAULT_POLICY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::FromRow)]
pub struct BreakPolicy {
    #[sqlx(rename = "maxMinutesLunch")]
    pub lunch: i32,
    #[sqlx(rename = "maxMinutesPrayer")]
    pub prayer: i32,
    #[sqlx(rename = "maxMinutesCoffee")]
    pub coffee: i32,
    #[sqlx(rename = "maxMinutesOther")]
    pub other: i32,
    #[sqlx(rename = "countTowardWorkHours")]
    pub count_toward_work_hours: bool,
}

pub const DEFAULT_POLICY: BreakPolicy = BreakPolicy {
    lunch: 60,
    prayer: 30,
    coffee: 15,
    other: 60,
    count_toward_work_hours: false,
};

impl BreakPolicy {
    /// `getMaxMinutesForType`.
    pub fn max_minutes(&self, kind: &str) -> i32 {
        match kind {
            "lunch" => self.lunch,
            "prayer" => self.prayer,
            "coffee" => self.coffee,
            _ => self.other,
        }
    }
}

/// `getPolicy(companyId)`.
pub async fn get_policy(db: &PgPool, company_id: i32) -> sqlx::Result<BreakPolicy> {
    let row: Option<BreakPolicy> = sqlx::query_as(
        r#"SELECT "maxMinutesLunch", "maxMinutesPrayer", "maxMinutesCoffee", "maxMinutesOther",
                  "countTowardWorkHours"
             FROM "mobileBreakPolicy" WHERE "companyId" = $1"#,
    )
    .bind(company_id)
    .fetch_optional(db)
    .await?;
    Ok(row.unwrap_or(DEFAULT_POLICY))
}

/// Shift aktif (kolom yang dipakai jalur mobile).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ActiveShift {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    #[sqlx(rename = "clockInAt")]
    pub clock_in_at: NaiveDateTime,
    #[sqlx(rename = "clockInVerified")]
    pub clock_in_verified: String,
    pub note: Option<String>,
}

/// `findFirst({ where: activeShiftWhere(userId, companyId) })` — Node tanpa `orderBy`.
async fn find_active_shift(
    db: &PgPool,
    user_id: i32,
    company_id: i32,
) -> sqlx::Result<Option<ActiveShift>> {
    sqlx::query_as(
        r#"SELECT id, uuid::text AS uuid, "companyId", "clockInAt",
                  "clockInVerified"::text AS "clockInVerified", note
             FROM "mobileShift"
            WHERE "userId" = $1 AND "companyId" = $2 AND status = 'active'
            LIMIT 1"#,
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(db)
    .await
}

/// `isFromPreviousDay(clockInAt, now, timeZone)`: clock-in sebelum awal hari ini (zona company,
/// `moment.tz(now, tz || 'UTC').startOf('day')`).
pub fn is_from_previous_day(
    clock_in_at: NaiveDateTime,
    now: NaiveDateTime,
    tz: Option<&str>,
) -> bool {
    let zone = mz_core::tz::zone_or_utc(tz);
    let local_date = zone.from_utc_datetime(&now).date_naive();
    let midnight = local_date.and_hms_opt(0, 0, 0).unwrap_or_default();
    let day_start = zone
        .from_local_datetime(&midnight)
        .earliest()
        .map_or(midnight, |d| d.naive_utc());
    clock_in_at < day_start
}

/// `isPreviousDayShift(shift)` (membaca `company.timeZone` milik shift).
async fn is_previous_day_shift(db: &PgPool, shift: &ActiveShift) -> sqlx::Result<bool> {
    let tz: Option<String> = sqlx::query_scalar(r#"SELECT "timeZone" FROM company WHERE id = $1"#)
        .bind(shift.company_id)
        .fetch_optional(db)
        .await?;
    Ok(is_from_previous_day(
        shift.clock_in_at,
        now_naive(),
        tz.as_deref(),
    ))
}

/// `abandonPreviousDayShift(tx, shiftId, now)`: `false` = sudah ditutup request lain. Dipakai
/// juga absensi otomatis kunjungan toko (`mz-store-visits`).
pub async fn abandon_previous_day_shift(
    tx: &mut Transaction<'_, Postgres>,
    shift_id: i32,
    now: NaiveDateTime,
) -> sqlx::Result<bool> {
    let n = sqlx::query(
        r#"UPDATE "mobileShift" SET status = 'auto_closed', "updatedAt" = $2
            WHERE id = $1 AND status = 'active'"#,
    )
    .bind(shift_id)
    .bind(now)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Ok(false);
    }
    sqlx::query(
        r#"UPDATE "mobileBreak" SET status = 'closed', "updatedAt" = $2
            WHERE "shiftId" = $1 AND status = 'active'"#,
    )
    .bind(shift_id)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(true)
}

/// `GpsDataDto`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gps {
    pub lat: f64,
    pub lng: f64,
    pub accuracy: f64,
    pub is_mock: bool,
}

/// `parseGps(raw)`. Galat bentuk di dalam `try` ikut ditangkap `catch` Node, jadi pesannya selalu
/// `Invalid GPS JSON`.
pub fn parse_gps(raw: Option<&str>) -> ApiResult<Option<Gps>> {
    let Some(raw) = raw.filter(|r| !r.is_empty()) else {
        return Ok(None);
    };
    let invalid = || ApiError::bad_request("Invalid GPS JSON");
    let v: Value = serde_json::from_str(raw).map_err(|_| invalid())?;
    let num = |k: &str| v.get(k).and_then(Value::as_f64);
    match (
        num("lat"),
        num("lng"),
        num("accuracy"),
        v.get("isMock").and_then(Value::as_bool),
    ) {
        (Some(lat), Some(lng), Some(accuracy), Some(is_mock)) => Ok(Some(Gps {
            lat,
            lng,
            accuracy,
            is_mock,
        })),
        _ => Err(invalid()),
    }
}

/// `parseFaceData(raw)`: objek JSON (bukan array/null), pesan selalu `Invalid face data JSON`.
pub fn parse_face_data(raw: Option<&str>) -> ApiResult<Option<Value>> {
    let Some(raw) = raw.filter(|r| !r.is_empty()) else {
        return Ok(None);
    };
    match serde_json::from_str::<Value>(raw) {
        Ok(v @ Value::Object(_)) => Ok(Some(v)),
        _ => Err(ApiError::bad_request("Invalid face data JSON")),
    }
}

/// `new Date(ms)` → `None` bila di luar jangkauan Date (Invalid Date → Prisma melempar → 500).
fn js_date_from_ms(ms: f64) -> ApiResult<NaiveDateTime> {
    let t = ms.trunc();
    if !t.is_finite() || t.abs() > 8.64e15 {
        return Err(ApiError::internal(anyhow::anyhow!("Invalid Date")));
    }
    #[allow(clippy::cast_possible_truncation)]
    JsDate::from_millis(t as i64)
        .map(|d| d.0)
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("Invalid Date")))
}

fn ms(dt: NaiveDateTime) -> i64 {
    dt.and_utc().timestamp_millis()
}

/// `assertOutsideNegativeZones(companyId, gps, requireLocation)`.
async fn assert_outside_negative_zones(
    db: &PgPool,
    company_id: i32,
    gps: Option<Gps>,
    require_location: bool,
) -> ApiResult<()> {
    let usable = gps.filter(|g| !g.is_mock);
    let Some(gps) = usable else {
        if !require_location {
            return Ok(());
        }
        let zones = geo::active_negative_zones(db, company_id).await?;
        if !zones.is_empty() {
            return Err(ApiError::coded(
                StatusCode::FORBIDDEN,
                "CLOCK_LOCATION_REQUIRED",
                "A valid (non-mock) location is required because this company has negative zones configured",
            ));
        }
        return Ok(());
    };
    let zones = geo::active_negative_zones(db, company_id).await?;
    if let Some((zone, distance)) = geo::containing_zone(&zones, gps.lat, gps.lng) {
        return Err(ApiError::body(
            StatusCode::FORBIDDEN,
            json!({
                "code": "CLOCK_INSIDE_NEGATIVE_ZONE",
                "message": "You are inside a negative zone and cannot clock in or out",
                "details": {
                    "negativeZone": {
                        "id": zone.uuid,
                        "name": zone.name,
                        "radiusMeters": zone.radius_meters,
                    },
                    "distanceMeters": distance,
                },
            }),
        ));
    }
    Ok(())
}

fn already_in() -> ApiError {
    ApiError::coded(
        StatusCode::CONFLICT,
        "CLOCK_ALREADY_IN",
        "An active shift already exists",
    )
}

#[derive(sqlx::FromRow)]
struct ClockCompany {
    #[sqlx(rename = "timeZone")]
    time_zone: String,
    #[sqlx(rename = "mobileBlockMockLocation")]
    block_mock: bool,
    #[sqlx(rename = "mobileRequireSelfie")]
    require_selfie: bool,
}

/// `selfies/<companyId>/<userId>/<uuid>[_out].jpg` → GCS (`filesService.uploadBuffer`).
async fn upload_selfie(
    state: &AppState,
    company_id: i32,
    user_id: i32,
    photo: &Photo,
    suffix: &str,
) -> ApiResult<String> {
    let key = format!(
        "selfies/{company_id}/{user_id}/{}{suffix}.jpg",
        uuid::Uuid::new_v4()
    );
    mz_files::upload_buffer(state, &key, photo.bytes.clone(), &photo.mime).await?;
    Ok(key)
}

/// Node: `clockIn` (:243).
pub async fn clock_in(
    state: &AppState,
    user_id: i32,
    dto: &ClockInDto,
    photo: Option<&Photo>,
) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = mz_core::company::require_company_id(db, user_id).await?;
    let (company, existing) = tokio::try_join!(
        sqlx::query_as::<_, ClockCompany>(
            r#"SELECT "timeZone", "mobileBlockMockLocation", "mobileRequireSelfie"
                 FROM company WHERE id = $1"#,
        )
        .bind(company_id)
        .fetch_one(db),
        find_active_shift(db, user_id, company_id),
    )?;

    // Shift aktif hari sebelumnya tidak menghalangi; ditutup di transaksi create.
    let previous_day = existing
        .as_ref()
        .filter(|s| is_from_previous_day(s.clock_in_at, now_naive(), Some(&company.time_zone)));
    if existing.is_some() && previous_day.is_none() {
        return Err(already_in());
    }
    let previous_day_id = previous_day.map(|s| s.id);

    let gps = parse_gps(dto.gps.as_deref())?;
    let face_data = parse_face_data(dto.face_data.as_deref())?;

    if gps.is_some_and(|g| g.is_mock) && company.block_mock {
        return Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            "CLOCK_MOCK_LOCATION_BLOCKED",
            "Mock location is not allowed",
        ));
    }
    if company.require_selfie && photo.is_none() {
        return Err(ApiError::coded(
            StatusCode::BAD_REQUEST,
            "CLOCK_SELFIE_REQUIRED",
            "Selfie photo is required",
        ));
    }

    assert_outside_negative_zones(db, company_id, gps, true).await?;

    let mut clock_in_geofence_id: Option<i32> = None;
    let mut verification = "unverified";
    let geofences = geo::active_geofences_for(db, company_id, user_id).await?;
    let has_enforced = geofences.iter().any(|g| g.enforced);
    let mut nearest_id: Option<i32> = None;

    match gps.filter(|g| !g.is_mock) {
        Some(g) => {
            let with_distance = geo::by_distance(&geofences, g.lat, g.lng);
            nearest_id = with_distance.first().map(|(gf, _)| gf.id);
            if let Some((gf, _)) = with_distance
                .iter()
                .find(|(gf, d)| *d <= i64::from(gf.radius_meters))
            {
                clock_in_geofence_id = Some(gf.id);
                verification = "verified";
            }
            // Ada geofence enforced: WAJIB di dalam salah satu geofence ENFORCED.
            if has_enforced
                && !with_distance
                    .iter()
                    .any(|(gf, d)| gf.enforced && *d <= i64::from(gf.radius_meters))
            {
                let nearest_enforced = with_distance.iter().find(|(gf, _)| gf.enforced);
                let mut body = json!({
                    "code": "CLOCK_OUTSIDE_GEOFENCE",
                    "message": "You are outside all enforced geofences",
                });
                if let (Some((gf, d)), Some(obj)) = (nearest_enforced, body.as_object_mut()) {
                    obj.insert(
                        "details".into(),
                        json!({
                            "nearestGeofence": {
                                "id": gf.uuid,
                                "name": gf.name,
                                "radiusMeters": gf.radius_meters,
                            },
                            "distanceMeters": d,
                        }),
                    );
                }
                return Err(ApiError::body(StatusCode::FORBIDDEN, body));
            }
        }
        None => {
            if gps.is_some_and(|g| g.is_mock) {
                verification = "failed";
            }
            if has_enforced {
                return Err(ApiError::coded(
                    StatusCode::FORBIDDEN,
                    "CLOCK_LOCATION_REQUIRED",
                    "A valid (non-mock) location is required to clock in at an enforced geofence",
                ));
            }
        }
    }

    let selfie_key = match photo {
        Some(p) => Some(upload_selfie(state, company_id, user_id, p, "").await?),
        None => None,
    };

    let now = now_naive();
    let client_at = js_date_from_ms(dto.client_timestamp)?;
    // TOCTOU sama dengan Node: tanpa unique index "satu shift aktif per user".
    let mut tx = db.begin().await?;
    if let Some(prev) = previous_day_id
        && !abandon_previous_day_shift(&mut tx, prev, now).await?
    {
        return Err(already_in());
    }
    let (uuid, clock_in_at): (String, NaiveDateTime) = sqlx::query_as(
        r#"INSERT INTO "mobileShift"
             ("userId", "companyId", "clockInAt", "clockInClientAt", note, status, source,
              "clockInLat", "clockInLng", "clockInAccuracy", "clockInIsMock", "clockInGeofenceId",
              "clockInSelfieKey", "clockInVerified", "clockInFaceData")
           VALUES ($1, $2, $3, $4, $5, 'active', 'mobile', $6, $7, $8, $9, $10, $11,
                   $12::"PunchVerifStatus", $13)
           RETURNING uuid::text, "clockInAt""#,
    )
    .bind(user_id)
    .bind(company_id)
    .bind(now)
    .bind(client_at)
    .bind(dto.note.as_deref())
    .bind(gps.map(|g| g.lat))
    .bind(gps.map(|g| g.lng))
    .bind(gps.map(|g| g.accuracy))
    .bind(gps.is_some_and(|g| g.is_mock))
    .bind(clock_in_geofence_id)
    .bind(selfie_key.as_deref())
    .bind(verification)
    .bind(face_data)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    // `geofenceId` respons: geofence tempat clock-in; di luar semua geofence (hanya bila tak ada
    // yang enforced) → geofence kantor default: terdekat dari GPS, atau id terkecil.
    let inside = clock_in_geofence_id.is_some();
    let response_gf = if let Some(id) = clock_in_geofence_id {
        geofences.iter().find(|g| g.id == id)
    } else if !has_enforced {
        match nearest_id {
            Some(id) => geofences.iter().find(|g| g.id == id),
            None => geofences.iter().min_by_key(|g| g.id),
        }
    } else {
        None
    };

    Ok(json!({
        "shiftId": uuid,
        "clockInAt": ms(clock_in_at),
        "geofenceId": response_gf.map(|g| g.uuid.clone()),
        "insideGeofence": inside,
        "verificationStatus": verification,
    }))
}

fn not_in() -> ApiError {
    ApiError::coded(
        StatusCode::CONFLICT,
        "CLOCK_NOT_IN",
        "No active shift found",
    )
}

/// Tutup istirahat aktif shift (`endedAt`, `durationSeconds`, `closed`).
async fn close_active_breaks(
    tx: &mut Transaction<'_, Postgres>,
    shift_id: i32,
    now: NaiveDateTime,
) -> sqlx::Result<()> {
    let breaks: Vec<(i32, NaiveDateTime)> = sqlx::query_as(
        r#"SELECT id, "startedAt" FROM "mobileBreak" WHERE "shiftId" = $1 AND status = 'active'"#,
    )
    .bind(shift_id)
    .fetch_all(&mut **tx)
    .await?;
    for (id, started) in breaks {
        let dur = (ms(now) - ms(started)).div_euclid(1000);
        sqlx::query(
            r#"UPDATE "mobileBreak"
                  SET "endedAt" = $2, "durationSeconds" = $3, status = 'closed', "updatedAt" = $2
                WHERE id = $1"#,
        )
        .bind(id)
        .bind(now)
        .bind(i32::try_from(dur).unwrap_or(i32::MAX))
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Node: `clockOut` (:434).
pub async fn clock_out(
    state: &AppState,
    user_id: i32,
    dto: &ClockOutDto,
    photo: Option<&Photo>,
) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = mz_core::company::require_company_id(db, user_id).await?;
    let Some(shift) = find_active_shift(db, user_id, company_id).await? else {
        return Err(not_in());
    };
    if is_previous_day_shift(db, &shift).await? {
        return Err(not_in());
    }

    let gps = parse_gps(dto.gps.as_deref())?;
    assert_outside_negative_zones(db, shift.company_id, gps, false).await?;

    let now = now_naive();
    let selfie_key = match photo {
        Some(p) => Some(upload_selfie(state, shift.company_id, user_id, p, "_out").await?),
        None => None,
    };

    let mut verified = "unverified";
    let mut out_geofence: Option<i32> = None;
    if let Some(g) = gps {
        if g.is_mock {
            verified = "failed";
        } else {
            let geofences = geo::active_geofences_for(db, shift.company_id, user_id).await?;
            if let Some((gf, _)) = geo::by_distance(&geofences, g.lat, g.lng)
                .into_iter()
                .find(|(gf, d)| *d <= i64::from(gf.radius_meters))
            {
                out_geofence = Some(gf.id);
                verified = "verified";
            }
        }
    }

    // `dto.clientTimestamp ? new Date(...) : null` — 0 dianggap kosong.
    let client_at = match dto.client_timestamp.filter(|t| *t != 0.0) {
        Some(t) => Some(js_date_from_ms(t)?),
        None => None,
    };
    let duration =
        i32::try_from((ms(now) - ms(shift.clock_in_at)).div_euclid(1000)).unwrap_or(i32::MAX);

    let mut tx = db.begin().await?;
    close_active_breaks(&mut tx, shift.id, now).await?;
    sqlx::query(
        r#"UPDATE "mobileShift"
              SET "clockOutAt" = $2, "clockOutClientAt" = $3, "durationSeconds" = $4, status = 'closed',
                  "clockOutLat" = $5, "clockOutLng" = $6, "clockOutAccuracy" = $7, "clockOutIsMock" = $8,
                  "clockOutGeofenceId" = $9, "clockOutSelfieKey" = $10,
                  "clockOutVerified" = $11::"PunchVerifStatus", "updatedAt" = $2
            WHERE id = $1"#,
    )
    .bind(shift.id)
    .bind(now)
    .bind(client_at)
    .bind(duration)
    .bind(gps.map(|g| g.lat))
    .bind(gps.map(|g| g.lng))
    .bind(gps.map(|g| g.accuracy))
    .bind(gps.map(|g| g.is_mock))
    .bind(out_geofence)
    .bind(selfie_key.as_deref())
    .bind(verified)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let (clock_out_at, duration_seconds): (Option<NaiveDateTime>, Option<i32>) = sqlx::query_as(
        r#"SELECT "clockOutAt", "durationSeconds" FROM "mobileShift" WHERE id = $1"#,
    )
    .bind(shift.id)
    .fetch_one(db)
    .await?;
    let total_break: i64 = sqlx::query_scalar(
        r#"SELECT COALESCE(SUM(COALESCE("durationSeconds", 0)), 0)::bigint
             FROM "mobileBreak" WHERE "shiftId" = $1 AND status IN ('closed', 'exceeded')"#,
    )
    .bind(shift.id)
    .fetch_one(db)
    .await?;

    Ok(json!({
        "shiftId": shift.uuid,
        "clockOutAt": clock_out_at.map(ms),
        "durationSeconds": duration_seconds,
        "totalBreakSeconds": total_break,
    }))
}

#[derive(sqlx::FromRow)]
struct ActiveBreak {
    id: i32,
    uuid: String,
    #[sqlx(rename = "type")]
    kind: String,
    #[sqlx(rename = "customLabel")]
    custom_label: Option<String>,
    #[sqlx(rename = "startedAt")]
    started_at: NaiveDateTime,
}

async fn find_active_break(db: &PgPool, shift_id: i32) -> sqlx::Result<Option<ActiveBreak>> {
    sqlx::query_as(
        r#"SELECT id, uuid::text AS uuid, type::text AS "type", "customLabel", "startedAt"
             FROM "mobileBreak" WHERE "shiftId" = $1 AND status = 'active'
            ORDER BY id LIMIT 1"#,
    )
    .bind(shift_id)
    .fetch_optional(db)
    .await
}

/// Node: `getCurrent` (:564). GET ini tidak menulis.
pub async fn get_current(state: &AppState, user_id: i32) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = mz_core::company::require_company_id(db, user_id).await?;
    let Some(shift) = find_active_shift(db, user_id, company_id).await? else {
        return Ok(Value::Null);
    };
    let active_break = find_active_break(db, shift.id).await?;
    if is_previous_day_shift(db, &shift).await? {
        return Ok(Value::Null);
    }
    let now = ms(now_naive());
    let elapsed = (now - ms(shift.clock_in_at)).div_euclid(1000);
    let active_break = match active_break {
        Some(br) => {
            let policy = get_policy(db, shift.company_id).await?;
            json!({
                "breakId": br.uuid,
                "type": br.kind,
                "startedAt": ms(br.started_at),
                "elapsedSeconds": (now - ms(br.started_at)).div_euclid(1000),
                "maxMinutes": policy.max_minutes(&br.kind),
            })
        }
        None => Value::Null,
    };
    Ok(json!({
        "shiftId": shift.uuid,
        "clockInAt": ms(shift.clock_in_at),
        "elapsedSeconds": elapsed,
        "verificationStatus": shift.clock_in_verified,
        "note": shift.note,
        "activeBreak": active_break,
    }))
}

/// Node: `autoClose` (:612).
pub async fn auto_close(
    state: &AppState,
    user_id: i32,
    threshold_hours: Option<f64>,
) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = mz_core::company::require_company_id(db, user_id).await?;
    let threshold = threshold_hours.unwrap_or(16.0);
    #[allow(clippy::cast_precision_loss)]
    let cutoff = js_date_from_ms(ms(now_naive()) as f64 - threshold * 3600.0 * 1000.0)?;
    let now = now_naive();
    let stale: Vec<(i32, String, NaiveDateTime)> = sqlx::query_as(
        r#"SELECT id, uuid::text, "clockInAt" FROM "mobileShift"
            WHERE "companyId" = $1 AND status = 'active' AND "clockInAt" < $2"#,
    )
    .bind(company_id)
    .bind(cutoff)
    .fetch_all(db)
    .await?;
    if stale.is_empty() {
        return Ok(json!({ "closedCount": 0, "shiftIds": [] }));
    }
    let mut tx = db.begin().await?;
    for (id, _, clock_in_at) in &stale {
        close_active_breaks(&mut tx, *id, now).await?;
        let duration =
            i32::try_from((ms(now) - ms(*clock_in_at)).div_euclid(1000)).unwrap_or(i32::MAX);
        sqlx::query(
            r#"UPDATE "mobileShift"
                  SET "clockOutAt" = $2, "durationSeconds" = $3, status = 'auto_closed', "updatedAt" = $2
                WHERE id = $1"#,
        )
        .bind(id)
        .bind(now)
        .bind(duration)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(json!({
        "closedCount": stale.len(),
        "shiftIds": stale.iter().map(|(_, u, _)| u.clone()).collect::<Vec<_>>(),
    }))
}

/// `GPS_STATUS_CLOCK_SKEW_MS`: toleransi jam device terhadap jam server. Laporan lebih dari ini
/// di depan jam server ditolak 400; lebih dari ini sebelum clock-in dianggap milik shift lain.
const GPS_STATUS_CLOCK_SKEW_MS: i64 = 5 * 60 * 1000;

#[derive(sqlx::FromRow)]
struct GpsStatusUpdated {
    #[sqlx(rename = "gpsStatus")]
    gps_status: String,
    #[sqlx(rename = "gpsStatusUpdatedAt")]
    gps_status_updated_at: NaiveDateTime,
    #[sqlx(rename = "gpsLastDisabledAt")]
    gps_last_disabled_at: Option<NaiveDateTime>,
    #[sqlx(rename = "gpsDisabledCount")]
    gps_disabled_count: i32,
}

fn gps_not_logged(reason: &str) -> Value {
    json!({ "logged": false, "reason": reason })
}

/// Node: `reportGpsStatus` — status GPS/izin lokasi dari device selama shift aktif.
///
/// TOCTOU: shift dibaca dulu (cek shift kemarin & batas clock-in), lalu ditulis dengan SATU
/// `UPDATE` bersyarat — `status = 'active'` dan `timestamp` lebih baru dari laporan tersimpan.
/// Clock-out/auto-close yang menyela membuat UPDATE tidak mengenai baris, dan laporan
/// bersamaan/tidak berurutan tidak saling menimpa ke arah yang lebih lama. Kenaikan
/// `gpsDisabledCount` dihitung dari nilai lama baris yang sama, jadi retry tidak dihitung dua kali.
pub async fn report_gps_status(
    state: &AppState,
    user_id: i32,
    dto: &GpsStatusDto,
) -> ApiResult<Value> {
    if dto.timestamp > ms(now_naive()) + GPS_STATUS_CLOCK_SKEW_MS {
        return Err(ApiError::coded(
            StatusCode::BAD_REQUEST,
            "TIMESTAMP_IN_FUTURE",
            "timestamp is ahead of server time",
        ));
    }
    let db = &state.db;
    let company_id = mz_core::company::require_company_id(db, user_id).await?;
    let Some(shift) = find_active_shift(db, user_id, company_id).await? else {
        return Ok(gps_not_logged("NO_ACTIVE_SHIFT"));
    };
    if is_previous_day_shift(db, &shift).await? {
        return Ok(gps_not_logged("NO_ACTIVE_SHIFT"));
    }
    // Laporan offline dari shift sebelumnya yang baru terkirim setelah clock-in baru.
    if dto.timestamp < ms(shift.clock_in_at) - GPS_STATUS_CLOCK_SKEW_MS {
        return Ok(gps_not_logged("NO_ACTIVE_SHIFT"));
    }

    #[allow(clippy::cast_precision_loss)]
    let reported_at = js_date_from_ms(dto.timestamp as f64)?;
    let (lat, lng, location_at) = match dto.location {
        #[allow(clippy::cast_precision_loss)]
        Some((lat, lng, at)) => (Some(lat), Some(lng), Some(js_date_from_ms(at as f64)?)),
        None => (None, None, None),
    };

    // Transisi ke status buruk = status tersimpan masih ok / belum ada laporan. Pindah antar
    // status buruk adalah kejadian yang sama. Ekspresi SET Postgres membaca nilai LAMA baris.
    let mut qb = sqlx::QueryBuilder::<Postgres>::new(r#"UPDATE "mobileShift" SET "#);
    if dto.status != "ok" {
        const BECAME_BAD: &str = r#"("gpsStatus" IS NULL OR "gpsStatus" = 'ok')"#;
        qb.push(format!(
            r#""gpsDisabledCount" = "gpsDisabledCount" + CASE WHEN {BECAME_BAD} THEN 1 ELSE 0 END, "#
        ));
        qb.push(format!(
            r#""gpsLastDisabledAt" = CASE WHEN {BECAME_BAD} THEN "#
        ))
        .push_bind(reported_at)
        .push(r#"::timestamp(3) ELSE "gpsLastDisabledAt" END, "#);
        qb.push(format!(
            r#""gpsLastDisabledLat" = CASE WHEN {BECAME_BAD} THEN "#
        ))
        .push_bind(lat)
        .push(r#"::double precision ELSE "gpsLastDisabledLat" END, "#);
        qb.push(format!(
            r#""gpsLastDisabledLng" = CASE WHEN {BECAME_BAD} THEN "#
        ))
        .push_bind(lng)
        .push(r#"::double precision ELSE "gpsLastDisabledLng" END, "#);
        qb.push(format!(
            r#""gpsLastDisabledLocationAt" = CASE WHEN {BECAME_BAD} THEN "#
        ))
        .push_bind(location_at)
        .push(r#"::timestamp(3) ELSE "gpsLastDisabledLocationAt" END, "#);
    }
    qb.push(r#""gpsStatus" = "#)
        .push_bind(dto.status.as_str())
        .push(r#"::"MobileShiftGpsStatus", "gpsStatusUpdatedAt" = "#)
        .push_bind(reported_at)
        .push(r#" WHERE id = "#)
        .push_bind(shift.id)
        .push(r#" AND status = 'active' AND ("gpsStatusUpdatedAt" IS NULL OR "gpsStatusUpdatedAt" < "#)
        .push_bind(reported_at)
        .push(
            r#") RETURNING "gpsStatus"::text AS "gpsStatus", "gpsStatusUpdatedAt",
                 "gpsLastDisabledAt", "gpsDisabledCount""#,
        );
    let row: Option<GpsStatusUpdated> = qb.build_query_as().fetch_optional(db).await?;

    let Some(row) = row else {
        // Bedakan "shift ditutup di sela baca & tulis" dari "sudah ada laporan lebih baru".
        let current: Option<String> =
            sqlx::query_scalar(r#"SELECT status::text FROM "mobileShift" WHERE id = $1"#)
                .bind(shift.id)
                .fetch_optional(db)
                .await?;
        return Ok(gps_not_logged(if current.as_deref() == Some("active") {
            "STALE"
        } else {
            "NO_ACTIVE_SHIFT"
        }));
    };
    Ok(json!({
        "logged": true,
        "shiftId": shift.uuid,
        "gpsStatus": row.gps_status,
        "gpsStatusUpdatedAt": ms(row.gps_status_updated_at),
        "gpsLastDisabledAt": row.gps_last_disabled_at.map(ms),
        "gpsDisabledCount": row.gps_disabled_count,
    }))
}

/// Shift aktif yang bukan dari hari sebelumnya, atau `None`.
async fn current_shift(db: &PgPool, user_id: i32) -> ApiResult<Option<ActiveShift>> {
    let company_id = mz_core::company::require_company_id(db, user_id).await?;
    let Some(shift) = find_active_shift(db, user_id, company_id).await? else {
        return Ok(None);
    };
    if is_previous_day_shift(db, &shift).await? {
        return Ok(None);
    }
    Ok(Some(shift))
}

/// Node: `breakStart` (:669).
pub async fn break_start(state: &AppState, user_id: i32, dto: &BreakStartDto) -> ApiResult<Value> {
    let db = &state.db;
    let Some(shift) = current_shift(db, user_id).await? else {
        return Err(ApiError::coded(
            StatusCode::BAD_REQUEST,
            "BREAK_NO_ACTIVE_SHIFT",
            "No active shift found",
        ));
    };
    if find_active_break(db, shift.id).await?.is_some() {
        return Err(ApiError::coded(
            StatusCode::CONFLICT,
            "BREAK_ALREADY_ACTIVE",
            "A break is already active",
        ));
    }
    if dto.kind == "other" && dto.custom_label.as_deref().is_none_or(str::is_empty) {
        return Err(ApiError::coded(
            StatusCode::BAD_REQUEST,
            "BREAK_LABEL_REQUIRED",
            "customLabel is required when type is \"other\"",
        ));
    }
    let policy = get_policy(db, shift.company_id).await?;
    let max_minutes = policy.max_minutes(&dto.kind);
    let now = now_naive();
    let (uuid, kind, started): (String, String, NaiveDateTime) = sqlx::query_as(
        r#"INSERT INTO "mobileBreak" ("shiftId", "userId", type, "customLabel", "startedAt", status)
           VALUES ($1, $2, $3::"MobileBreakType", $4, $5, 'active')
           RETURNING uuid::text, type::text, "startedAt""#,
    )
    .bind(shift.id)
    .bind(user_id)
    .bind(&dto.kind)
    .bind(dto.custom_label.as_deref())
    .bind(now)
    .fetch_one(db)
    .await?;
    Ok(json!({
        "breakId": uuid,
        "type": kind,
        "startedAt": ms(started),
        "maxMinutes": max_minutes,
    }))
}

/// Node: `breakEnd` (:723).
pub async fn break_end(state: &AppState, user_id: i32) -> ApiResult<Value> {
    let db = &state.db;
    let Some(shift) = current_shift(db, user_id).await? else {
        return Err(ApiError::coded(
            StatusCode::BAD_REQUEST,
            "BREAK_NOT_ACTIVE",
            "No active shift found",
        ));
    };
    let Some(br) = find_active_break(db, shift.id).await? else {
        return Err(ApiError::coded(
            StatusCode::BAD_REQUEST,
            "BREAK_NOT_ACTIVE",
            "No active break found",
        ));
    };
    let now = now_naive();
    let duration = (ms(now) - ms(br.started_at)).div_euclid(1000);
    let policy = get_policy(db, shift.company_id).await?;
    let exceeded = duration > i64::from(policy.max_minutes(&br.kind)) * 60;
    let (uuid, ended): (String, Option<NaiveDateTime>) = sqlx::query_as(
        r#"UPDATE "mobileBreak"
              SET "endedAt" = $2, "durationSeconds" = $3,
                  status = $4::"MobileBreakStatus", "updatedAt" = $2
            WHERE id = $1
        RETURNING uuid::text, "endedAt""#,
    )
    .bind(br.id)
    .bind(now)
    .bind(i32::try_from(duration).unwrap_or(i32::MAX))
    .bind(if exceeded { "exceeded" } else { "closed" })
    .fetch_one(db)
    .await?;
    Ok(json!({
        "breakId": uuid,
        "endedAt": ended.map(ms),
        "durationSeconds": duration,
        "exceeded": exceeded,
    }))
}

/// Node: `getBreakCurrent` (:773).
pub async fn break_current(state: &AppState, user_id: i32) -> ApiResult<Value> {
    let db = &state.db;
    let Some(shift) = current_shift(db, user_id).await? else {
        return Ok(Value::Null);
    };
    let Some(br) = find_active_break(db, shift.id).await? else {
        return Ok(Value::Null);
    };
    let policy = get_policy(db, shift.company_id).await?;
    Ok(json!({
        "breakId": br.uuid,
        "type": br.kind,
        "customLabel": br.custom_label,
        "startedAt": ms(br.started_at),
        "elapsedSeconds": (ms(now_naive()) - ms(br.started_at)).div_euclid(1000),
        "maxMinutes": policy.max_minutes(&br.kind),
    }))
}

/// Node: `getBreakPolicy` (:1752).
pub async fn break_policy(state: &AppState, user_id: i32) -> ApiResult<Value> {
    let company_id = mz_core::company::require_company_id(&state.db, user_id).await?;
    let p = get_policy(&state.db, company_id).await?;
    Ok(json!({
        "maxMinutes": {
            "lunch": p.lunch,
            "prayer": p.prayer,
            "coffee": p.coffee,
            "other": p.other,
        },
        "countTowardWorkHours": p.count_toward_work_hours,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        mz_core::time::parse_js_date(s).unwrap_or_default()
    }

    #[test]
    fn previous_day_uses_company_zone() {
        // 2026-06-02 01:00 WIB = 2026-06-01T18:00Z; awal hari WIB = 2026-06-01T17:00Z.
        let now = at("2026-06-01T18:00:00Z");
        assert!(is_from_previous_day(
            at("2026-06-01T16:59:59Z"),
            now,
            Some("Asia/Jakarta")
        ));
        assert!(!is_from_previous_day(
            at("2026-06-01T17:00:00Z"),
            now,
            Some("Asia/Jakarta")
        ));
        // UTC: awal hari 2026-06-01T00:00Z.
        assert!(!is_from_previous_day(
            at("2026-06-01T01:00:00Z"),
            now,
            Some("")
        ));
        assert!(is_from_previous_day(at("2026-05-31T23:00:00Z"), now, None));
        // Zona tak dikenal → UTC (moment-timezone).
        assert!(!is_from_previous_day(
            at("2026-06-01T00:00:00Z"),
            now,
            Some("Bogus/X")
        ));
    }

    #[test]
    fn gps_parsing() {
        assert_eq!(parse_gps(None).ok(), Some(None));
        assert_eq!(parse_gps(Some("")).ok(), Some(None));
        assert_eq!(
            parse_gps(Some(
                r#"{"lat":-6.2,"lng":106.8,"accuracy":10,"isMock":false}"#
            ))
            .ok(),
            Some(Some(Gps {
                lat: -6.2,
                lng: 106.8,
                accuracy: 10.0,
                is_mock: false
            }))
        );
        for bad in [
            "x",
            "null",
            r#"{"lat":"1","lng":1,"accuracy":1,"isMock":false}"#,
        ] {
            assert!(
                matches!(parse_gps(Some(bad)), Err(ApiError::Message { ref message, .. }) if message == "Invalid GPS JSON"),
                "{bad}"
            );
        }
        assert!(parse_face_data(Some("[1]")).is_err());
        assert!(parse_face_data(Some("null")).is_err());
        assert!(matches!(parse_face_data(Some("{\"a\":1}")), Ok(Some(_))));
    }

    #[test]
    fn policy_max_minutes() {
        assert_eq!(DEFAULT_POLICY.max_minutes("coffee"), 15);
        assert_eq!(DEFAULT_POLICY.max_minutes("weird"), 60);
    }
}
