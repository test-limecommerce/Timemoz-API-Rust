//! Daftar & detail absensi (Node: `src/mobile-clock/mobile-clock.service.ts:960-1977`):
//! `getAdminAttendance`, `getMyAttendance`, `getAdminAttendanceDetail`,
//! `getAdminAttendanceSelfie`, `fetchAttendanceExportData`.

use std::collections::{HashMap, HashSet};

use chrono::NaiveDateTime;
use mz_core::jsfmt::num;
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::body::AttendanceQuery;
use crate::schedule::{self, ScheduleGeofence};

fn ms(dt: NaiveDateTime) -> i64 {
    dt.and_utc().timestamp_millis()
}

fn opt_num(v: Option<f64>) -> Value {
    v.map_or(Value::Null, num)
}

/// `toAdminShiftStatus`: `auto_closed` dilaporkan `closed`.
pub fn admin_status(status: &str) -> &'static str {
    if status == "active" {
        "active"
    } else {
        "closed"
    }
}

/// Satu baris `mobileShift` (+ user) untuk daftar/detail.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ShiftRow {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "userId")]
    pub user_id: i32,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    #[sqlx(rename = "clockInAt")]
    pub clock_in_at: NaiveDateTime,
    #[sqlx(rename = "clockOutAt")]
    pub clock_out_at: Option<NaiveDateTime>,
    #[sqlx(rename = "durationSeconds")]
    pub duration_seconds: Option<i32>,
    pub note: Option<String>,
    pub status: String,
    #[sqlx(rename = "clockInLat")]
    pub in_lat: Option<f64>,
    #[sqlx(rename = "clockInLng")]
    pub in_lng: Option<f64>,
    #[sqlx(rename = "clockInAccuracy")]
    pub in_accuracy: Option<f64>,
    #[sqlx(rename = "clockInIsMock")]
    pub in_is_mock: bool,
    #[sqlx(rename = "clockInGeofenceId")]
    pub in_geofence_id: Option<i32>,
    #[sqlx(rename = "clockInSelfieKey")]
    pub in_selfie_key: Option<String>,
    #[sqlx(rename = "clockInVerified")]
    pub in_verified: String,
    #[sqlx(rename = "clockOutLat")]
    pub out_lat: Option<f64>,
    #[sqlx(rename = "clockOutLng")]
    pub out_lng: Option<f64>,
    #[sqlx(rename = "clockOutAccuracy")]
    pub out_accuracy: Option<f64>,
    #[sqlx(rename = "clockOutIsMock")]
    pub out_is_mock: Option<bool>,
    #[sqlx(rename = "clockOutGeofenceId")]
    pub out_geofence_id: Option<i32>,
    #[sqlx(rename = "clockOutSelfieKey")]
    pub out_selfie_key: Option<String>,
    #[sqlx(rename = "clockOutVerified")]
    pub out_verified: Option<String>,
    pub user_uuid: String,
    pub user_name: String,
    pub user_username: String,
    #[sqlx(flatten)]
    pub gps: ShiftGps,
}

/// Kolom pemantauan GPS/izin lokasi shift (`POST /mobile/clock/gps-status`).
#[derive(Debug, Clone, Default, sqlx::FromRow)]
pub struct ShiftGps {
    #[sqlx(rename = "gpsStatus")]
    pub status: Option<String>,
    #[sqlx(rename = "gpsStatusUpdatedAt")]
    pub updated_at: Option<NaiveDateTime>,
    #[sqlx(rename = "gpsLastDisabledAt")]
    pub last_disabled_at: Option<NaiveDateTime>,
    #[sqlx(rename = "gpsLastDisabledLat")]
    pub last_disabled_lat: Option<f64>,
    #[sqlx(rename = "gpsLastDisabledLng")]
    pub last_disabled_lng: Option<f64>,
    #[sqlx(rename = "gpsLastDisabledLocationAt")]
    pub last_disabled_location_at: Option<NaiveDateTime>,
    #[sqlx(rename = "gpsDisabledCount")]
    pub disabled_count: i32,
}

/// `{ lat, lng, at }` posisi last-known saat GPS terakhir dimatikan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GpsLocation {
    pub lat: f64,
    pub lng: f64,
    pub at_ms: Option<i64>,
}

/// Objek `gps` di daftar, detail & ekspor attendance admin (`toAdminGps`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AdminGps {
    pub status: Option<String>,
    pub updated_at_ms: Option<i64>,
    pub last_disabled_at_ms: Option<i64>,
    pub last_disabled_location: Option<GpsLocation>,
    pub disabled_count: i64,
}

impl ShiftGps {
    pub fn admin(&self) -> AdminGps {
        AdminGps {
            status: self.status.clone(),
            updated_at_ms: self.updated_at.map(ms),
            last_disabled_at_ms: self.last_disabled_at.map(ms),
            last_disabled_location: self.last_disabled_lat.zip(self.last_disabled_lng).map(
                |(lat, lng)| GpsLocation {
                    lat,
                    lng,
                    at_ms: self.last_disabled_location_at.map(ms),
                },
            ),
            disabled_count: i64::from(self.disabled_count),
        }
    }
}

/// `attendance-gps-format.ts`: format kolom GPS ekspor attendance (Excel & PDF).
impl AdminGps {
    /// `gpsStatusLabel`: label status GPS terakhir; `-` bila device belum pernah melapor.
    pub fn status_label(&self) -> &str {
        match self.status.as_deref() {
            None => "-",
            Some("ok") => "OK",
            Some("gps_disabled") => "GPS Off",
            Some("permission_denied") => "Permission Denied",
            Some("permission_denied_forever") => "Permission Denied (Permanent)",
            Some(other) => other,
        }
    }

    /// `gpsOffSummary`: `-` (belum pernah melapor), `No` (melapor, tak pernah mati), atau
    /// `2x, 06-09 03:05` (jumlah kejadian + terakhir dimatikan, `MM-DD HH:mm` UTC).
    pub fn off_summary(&self) -> String {
        if self.disabled_count > 0 {
            let at = self
                .last_disabled_at_ms
                .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis);
            return match at {
                Some(d) => format!("{}x, {}", self.disabled_count, d.format("%m-%d %H:%M")),
                None => format!("{}x", self.disabled_count),
            };
        }
        if self.status.is_none() { "-" } else { "No" }.to_owned()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "status": self.status,
            "updatedAt": self.updated_at_ms,
            "lastDisabledAt": self.last_disabled_at_ms,
            "lastDisabledLocation": self.last_disabled_location.map_or(Value::Null, |l| json!({
                "lat": num(l.lat),
                "lng": num(l.lng),
                "at": l.at_ms,
            })),
            "disabledCount": self.disabled_count,
        })
    }
}

const SHIFT_COLUMNS: &str = r#"s.id, s.uuid::text AS uuid, s."userId", s."companyId", s."clockInAt", s."clockOutAt",
    s."durationSeconds", s.note, s.status::text AS status,
    s."clockInLat", s."clockInLng", s."clockInAccuracy", s."clockInIsMock", s."clockInGeofenceId",
    s."clockInSelfieKey", s."clockInVerified"::text AS "clockInVerified",
    s."clockOutLat", s."clockOutLng", s."clockOutAccuracy", s."clockOutIsMock", s."clockOutGeofenceId",
    s."clockOutSelfieKey", s."clockOutVerified"::text AS "clockOutVerified",
    u.uuid::text AS user_uuid, u.name AS user_name, u.username AS user_username,
    s."gpsStatus"::text AS "gpsStatus", s."gpsStatusUpdatedAt", s."gpsLastDisabledAt",
    s."gpsLastDisabledLat", s."gpsLastDisabledLng", s."gpsLastDisabledLocationAt",
    s."gpsDisabledCount""#;

/// Geofence untuk tampilan (`{ id, name, radiusMeters }`, + alamat & jadwal untuk daftar selfie).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct GfInfo {
    pub id: i32,
    pub uuid: String,
    pub name: String,
    pub address: Option<String>,
    #[sqlx(rename = "radiusMeters")]
    pub radius_meters: i32,
    #[sqlx(rename = "weeklySchedule")]
    pub weekly_schedule: Option<Value>,
    pub timezone: Option<String>,
}

impl GfInfo {
    fn short(&self) -> Value {
        json!({ "id": self.uuid, "name": self.name, "radiusMeters": self.radius_meters })
    }
}

/// Batch geofence untuk tampilan ("avoids N+1").
async fn geofence_map(db: &PgPool, shifts: &[ShiftRow]) -> sqlx::Result<HashMap<i32, GfInfo>> {
    let ids: Vec<i32> = shifts
        .iter()
        .flat_map(|s| [s.in_geofence_id, s.out_geofence_id])
        .flatten()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<GfInfo> = sqlx::query_as(
        r#"SELECT id, uuid::text AS uuid, name, address, "radiusMeters", "weeklySchedule", timezone
             FROM geofence WHERE id = ANY($1)"#,
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|g| (g.id, g)).collect())
}

/// Rentang `new Date(`${startDate}T00:00:00.000Z`)` .. `T23:59:59.999Z`. `None` = Invalid Date.
struct Range {
    start: Option<NaiveDateTime>,
    end: Option<NaiveDateTime>,
}

fn range_of(q: &AttendanceQuery) -> ApiResult<Range> {
    let parse = |s: String| mz_core::time::v8_date(&s);
    let r = Range {
        start: parse(format!("{}T00:00:00.000Z", q.start_date)),
        end: parse(format!("{}T23:59:59.999Z", q.end_date)),
    };
    if let (Some(s), Some(e)) = (r.start, r.end)
        && e < s
    {
        return Err(ApiError::bad_request(
            "endDate must be on or after startDate",
        ));
    }
    Ok(r)
}

/// Prisma menolak `Invalid Date` saat query dijalankan (500).
fn resolved(r: &Range) -> ApiResult<(NaiveDateTime, NaiveDateTime)> {
    match (r.start, r.end) {
        (Some(s), Some(e)) => Ok((s, e)),
        _ => Err(ApiError::internal(anyhow::anyhow!("Invalid Date"))),
    }
}

/// `geofence.findFirst({ where: { uuid: geofenceUuid, companyId } })` → id, atau 400.
async fn filter_geofence(
    db: &PgPool,
    company_id: i32,
    uuid: Option<&str>,
) -> ApiResult<Option<i32>> {
    let Some(uuid) = uuid else { return Ok(None) };
    let id: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM geofence WHERE uuid = $1::uuid AND "companyId" = $2 LIMIT 1"#,
    )
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(db)
    .await?;
    id.map(Some)
        .ok_or_else(|| ApiError::bad_request("Geofence not found"))
}

/// Filter daftar: company (+ user sendiri), rentang clock-in, user uuid, geofence in/out.
struct Filter<'a> {
    company_id: i32,
    own_user: Option<i32>,
    start: NaiveDateTime,
    end: NaiveDateTime,
    user_uuid: Option<&'a str>,
    geofence_id: Option<i32>,
    gps_off: bool,
}

impl Filter<'_> {
    fn push(&self, qb: &mut QueryBuilder<'_, Postgres>) {
        qb.push(r#" WHERE s."companyId" = "#)
            .push_bind(self.company_id);
        if let Some(uid) = self.own_user {
            qb.push(r#" AND s."userId" = "#).push_bind(uid);
        }
        qb.push(r#" AND s."clockInAt" >= "#)
            .push_bind(self.start)
            .push(r#" AND s."clockInAt" <= "#)
            .push_bind(self.end);
        if let Some(u) = self.user_uuid {
            qb.push(" AND u.uuid = ")
                .push_bind(u.to_owned())
                .push("::uuid");
        }
        if let Some(g) = self.geofence_id {
            qb.push(r#" AND (s."clockInGeofenceId" = "#)
                .push_bind(g)
                .push(r#" OR s."clockOutGeofenceId" = "#)
                .push_bind(g)
                .push(")");
        }
        if self.gps_off {
            qb.push(r#" AND s."gpsDisabledCount" > 0"#);
        }
    }
}

const FROM: &str = r#" FROM "mobileShift" s JOIN users u ON u.id = s."userId""#;

async fn list_shifts(
    db: &PgPool,
    f: &Filter<'_>,
    page: Option<(i64, i64)>,
    take: Option<i64>,
) -> sqlx::Result<Vec<ShiftRow>> {
    let mut qb = QueryBuilder::<Postgres>::new(format!("SELECT {SHIFT_COLUMNS}{FROM}"));
    f.push(&mut qb);
    qb.push(r#" ORDER BY s."clockInAt" DESC"#);
    if let Some((page, limit)) = page {
        qb.push(" LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind((page - 1).saturating_mul(limit));
    }
    if let Some(t) = take {
        qb.push(" LIMIT ").push_bind(t);
    }
    qb.build_query_as().fetch_all(db).await
}

async fn count_shifts(db: &PgPool, f: &Filter<'_>) -> sqlx::Result<i64> {
    let mut qb = QueryBuilder::<Postgres>::new(format!("SELECT COUNT(*){FROM}"));
    f.push(&mut qb);
    qb.build_query_scalar().fetch_one(db).await
}

/// Baris ringkasan (`allShiftsForSummary`).
#[derive(sqlx::FromRow)]
struct SummaryShift {
    #[sqlx(rename = "userId")]
    user_id: i32,
    #[sqlx(rename = "clockInAt")]
    clock_in_at: NaiveDateTime,
    #[sqlx(rename = "durationSeconds")]
    duration_seconds: Option<i32>,
    status: String,
    #[sqlx(rename = "clockInGeofenceId")]
    clock_in_geofence_id: Option<i32>,
}

async fn summary_shifts(
    db: &PgPool,
    company_id: i32,
    own_user: Option<i32>,
    start: NaiveDateTime,
    end: NaiveDateTime,
) -> sqlx::Result<Vec<SummaryShift>> {
    sqlx::query_as(
        r#"SELECT "userId", "clockInAt", "durationSeconds", status::text AS status, "clockInGeofenceId"
             FROM "mobileShift"
            WHERE "companyId" = $1 AND ($2::int IS NULL OR "userId" = $2)
              AND "clockInAt" >= $3 AND "clockInAt" <= $4"#,
    )
    .bind(company_id)
    .bind(own_user)
    .bind(start)
    .bind(end)
    .fetch_all(db)
    .await
}

/// Geofence aktif company untuk ringkasan admin (`applicableTo`, `applicableIds`, jadwal).
struct ActiveGf {
    sched: ScheduleGeofence,
    applicable_to: String,
    applicable_ids: Vec<i32>,
}

type GfRow = (i32, String, Vec<i32>, Option<Value>, Option<String>);

async fn active_company_geofences(db: &PgPool, company_id: i32) -> sqlx::Result<Vec<ActiveGf>> {
    let rows: Vec<GfRow> = sqlx::query_as(
        r#"SELECT id, "applicableTo"::text, COALESCE("applicableIds", '{}'), "weeklySchedule", timezone
             FROM geofence WHERE "companyId" = $1 AND "isActive" = true ORDER BY id"#,
    )
    .bind(company_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, applicable_to, applicable_ids, ws, tz)| ActiveGf {
            sched: ScheduleGeofence {
                id,
                weekly_schedule: ws,
                timezone: tz,
            },
            applicable_to,
            applicable_ids,
        })
        .collect())
}

/// Geofence untuk cek telat satu shift (admin): yang berlaku untuk user, atau geofence
/// clock-in-nya bila masih aktif.
fn admin_check_set(
    all: &[ActiveGf],
    user_id: i32,
    clock_in_gf: Option<i32>,
) -> Vec<&ScheduleGeofence> {
    if let Some(gf) = clock_in_gf.and_then(|id| all.iter().find(|g| g.sched.id == id)) {
        return vec![&gf.sched];
    }
    all.iter()
        .filter(|g| g.applicable_to == "all" || g.applicable_ids.contains(&user_id))
        .map(|g| &g.sched)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub total_present: usize,
    pub total_late: usize,
    pub total_absent: usize,
    pub average_work_seconds: Option<i64>,
}

/// `Math.round(total / count)`.
fn average(total: i64, count: i64) -> Option<i64> {
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    (count > 0).then(|| ((total as f64 / count as f64) + 0.5).floor() as i64)
}

fn counts_duration(s: &SummaryShift) -> bool {
    s.duration_seconds.is_some() && (s.status == "closed" || s.status == "auto_closed")
}

/// Ringkasan admin (company-wide; mengabaikan filter user/geofence).
async fn admin_summary(
    db: &PgPool,
    company_id: i32,
    shifts: &[SummaryShift],
    gfs: &[ActiveGf],
) -> ApiResult<Summary> {
    let assigned: HashSet<i32> = if gfs.iter().any(|g| g.applicable_to == "all") {
        // `userCompany.findMany({ where: { companyId } })` — tanpa filter status.
        let ids: Vec<i32> =
            sqlx::query_scalar(r#"SELECT "userId" FROM "userCompany" WHERE "companyId" = $1"#)
                .bind(company_id)
                .fetch_all(db)
                .await?;
        ids.into_iter().collect()
    } else {
        gfs.iter()
            .flat_map(|g| g.applicable_ids.iter().copied())
            .collect()
    };
    let present: HashSet<i32> = shifts.iter().map(|s| s.user_id).collect();
    let mut late = 0;
    let (mut total, mut count) = (0_i64, 0_i64);
    for s in shifts {
        let check = admin_check_set(gfs, s.user_id, s.clock_in_geofence_id);
        if schedule::late_seconds(s.clock_in_at, &check)?.is_some() {
            late += 1;
        }
        if counts_duration(s) {
            total += i64::from(s.duration_seconds.unwrap_or(0));
            count += 1;
        }
    }
    Ok(Summary {
        total_present: shifts.len(),
        total_late: late,
        total_absent: assigned.iter().filter(|id| !present.contains(id)).count(),
        average_work_seconds: average(total, count),
    })
}

fn gf_short(map: &HashMap<i32, GfInfo>, id: Option<i32>) -> Value {
    id.and_then(|i| map.get(&i))
        .map_or(Value::Null, GfInfo::short)
}

fn punch_in(s: &ShiftRow, map: &HashMap<i32, GfInfo>, with_selfie: bool) -> Value {
    let mut v = json!({
        "at": ms(s.clock_in_at),
        "lat": opt_num(s.in_lat),
        "lng": opt_num(s.in_lng),
        "accuracyMeters": opt_num(s.in_accuracy),
        "isMock": s.in_is_mock,
        "verificationStatus": s.in_verified,
        "geofence": gf_short(map, s.in_geofence_id),
    });
    if with_selfie && let Some(o) = v.as_object_mut() {
        o.insert(
            "hasSelfie".into(),
            Value::Bool(s.in_selfie_key.as_deref().is_some_and(|k| !k.is_empty())),
        );
    }
    v
}

fn punch_out(s: &ShiftRow, map: &HashMap<i32, GfInfo>, with_selfie: bool) -> Value {
    let Some(at) = s.clock_out_at else {
        return Value::Null;
    };
    let mut v = json!({
        "at": ms(at),
        "lat": opt_num(s.out_lat),
        "lng": opt_num(s.out_lng),
        "accuracyMeters": opt_num(s.out_accuracy),
        "isMock": s.out_is_mock.unwrap_or(false),
        "verificationStatus": s.out_verified,
        "geofence": gf_short(map, s.out_geofence_id),
    });
    if with_selfie && let Some(o) = v.as_object_mut() {
        o.insert(
            "hasSelfie".into(),
            Value::Bool(s.out_selfie_key.as_deref().is_some_and(|k| !k.is_empty())),
        );
    }
    v
}

fn user_of(s: &ShiftRow) -> Value {
    json!({ "id": s.user_uuid, "name": s.user_name, "username": s.user_username })
}

fn meta(total: i64, page: i64, limit: i64) -> Value {
    json!({
        "total": total,
        "page": page,
        "limit": limit,
        "totalPages": (total + limit - 1).div_euclid(limit.max(1)),
    })
}

/// Node: `getAdminAttendance` (:960).
pub async fn admin_attendance(
    state: &AppState,
    user_id: i32,
    q: &AttendanceQuery,
) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = mz_core::company::require_company_id(db, user_id).await?;
    let range = range_of(q)?;
    let geofence_id = filter_geofence(db, company_id, q.geofence_uuid.as_deref()).await?;
    let (start, end) = resolved(&range)?;
    let f = Filter {
        company_id,
        own_user: None,
        start,
        end,
        user_uuid: q.user_uuid.as_deref(),
        geofence_id,
        gps_off: q.gps_off,
    };
    let (shifts, total, all, gfs) = tokio::try_join!(
        list_shifts(db, &f, Some((q.page, q.limit)), None),
        count_shifts(db, &f),
        summary_shifts(db, company_id, None, start, end),
        active_company_geofences(db, company_id),
    )?;
    let summary = admin_summary(db, company_id, &all, &gfs).await?;
    let map = geofence_map(db, &shifts).await?;
    let data: Vec<Value> = shifts
        .iter()
        .map(|s| {
            json!({
                "shiftId": s.uuid,
                "user": user_of(s),
                "clockIn": punch_in(s, &map, true),
                "clockOut": punch_out(s, &map, true),
                "durationSeconds": s.duration_seconds,
                "status": admin_status(&s.status),
                "note": s.note,
                "gps": s.gps.admin().to_json(),
            })
        })
        .collect();
    Ok(json!({
        "startDate": q.start_date,
        "endDate": q.end_date,
        "summary": {
            "totalPresent": summary.total_present,
            "totalLate": summary.total_late,
            "totalAbsent": summary.total_absent,
            "averageWorkSeconds": summary.average_work_seconds,
        },
        "data": data,
        "meta": meta(total, q.page, q.limit),
    }))
}

/// Node: `getMyAttendance` (:1190).
pub async fn my_attendance(
    state: &AppState,
    user_id: i32,
    q: &AttendanceQuery,
) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = mz_core::company::require_company_id(db, user_id).await?;
    let range = range_of(q)?;
    let geofence_id = filter_geofence(db, company_id, q.geofence_uuid.as_deref()).await?;
    let (start, end) = resolved(&range)?;
    let f = Filter {
        company_id,
        own_user: Some(user_id),
        start,
        end,
        user_uuid: None,
        geofence_id,
        gps_off: false,
    };
    let applicable = async {
        let rows: Vec<(i32, Option<Value>, Option<String>)> = sqlx::query_as(
            r#"SELECT id, "weeklySchedule", timezone FROM geofence
                WHERE "companyId" = $1 AND "isActive" = true
                  AND ("applicableTo" = 'all' OR $2 = ANY("applicableIds"))
                ORDER BY id"#,
        )
        .bind(company_id)
        .bind(user_id)
        .fetch_all(db)
        .await?;
        Ok::<_, sqlx::Error>(
            rows.into_iter()
                .map(|(id, ws, tz)| ScheduleGeofence {
                    id,
                    weekly_schedule: ws,
                    timezone: tz,
                })
                .collect::<Vec<_>>(),
        )
    };
    let (shifts, total, all, gfs) = tokio::try_join!(
        list_shifts(db, &f, Some((q.page, q.limit)), None),
        count_shifts(db, &f),
        summary_shifts(db, company_id, Some(user_id), start, end),
        applicable,
    )?;
    let mut late = 0;
    let (mut total_work, mut count) = (0_i64, 0_i64);
    let all_refs: Vec<&ScheduleGeofence> = gfs.iter().collect();
    for s in &all {
        let check = match s
            .clock_in_geofence_id
            .and_then(|id| gfs.iter().find(|g| g.id == id))
        {
            Some(g) => vec![g],
            None => all_refs.clone(),
        };
        if schedule::late_seconds(s.clock_in_at, &check)?.is_some() {
            late += 1;
        }
        if counts_duration(s) {
            total_work += i64::from(s.duration_seconds.unwrap_or(0));
            count += 1;
        }
    }
    let map = geofence_map(db, &shifts).await?;
    let data: Vec<Value> = shifts
        .iter()
        .map(|s| {
            json!({
                "shiftId": s.uuid,
                "clockIn": punch_in(s, &map, true),
                "clockOut": punch_out(s, &map, true),
                "durationSeconds": s.duration_seconds,
                "status": s.status,
                "note": s.note,
            })
        })
        .collect();
    Ok(json!({
        "startDate": q.start_date,
        "endDate": q.end_date,
        "summary": {
            "totalPresent": all.len(),
            "totalLate": late,
            "averageWorkSeconds": average(total_work, count),
        },
        "data": data,
        "meta": meta(total, q.page, q.limit),
    }))
}

/// Ping geofence kedatangan datang beberapa detik sebelum clock-in; 15 menit memberi ruang
/// untuk HP yang masuk zona lebih awal tanpa menarik event jauh sebelum shift.
const EVENT_LEAD_MS: i64 = 15 * 60 * 1000;

/// `resolveEventWindow(shift)`: rentang event lokasi milik satu shift.
///
/// Awal: `clockInAt − EVENT_LEAD_MS`, tidak lebih awal dari `clockOutAt` shift sebelumnya
/// (+1 ms) supaya tak ada event yang tampil di dua shift.
/// Akhir: `clockOutAt`; bila kosong, yang paling awal dari sekarang, `clockInAt + 24 j`, dan
/// awal rentang shift berikutnya (clock-in berikutnya − `EVENT_LEAD_MS` − 1 ms).
async fn event_window(db: &PgPool, s: &ShiftRow) -> sqlx::Result<(NaiveDateTime, NaiveDateTime)> {
    let prev = async {
        sqlx::query_scalar::<_, Option<NaiveDateTime>>(
            r#"SELECT "clockOutAt" FROM "mobileShift"
                WHERE "userId" = $1 AND "companyId" = $2 AND "clockInAt" < $3
                ORDER BY "clockInAt" DESC LIMIT 1"#,
        )
        .bind(s.user_id)
        .bind(s.company_id)
        .bind(s.clock_in_at)
        .fetch_optional(db)
        .await
    };
    let next = async {
        if s.clock_out_at.is_some() {
            return Ok(None);
        }
        sqlx::query_scalar::<_, NaiveDateTime>(
            r#"SELECT "clockInAt" FROM "mobileShift"
                WHERE "userId" = $1 AND "companyId" = $2 AND "clockInAt" > $3
                ORDER BY "clockInAt" ASC LIMIT 1"#,
        )
        .bind(s.user_id)
        .bind(s.company_id)
        .bind(s.clock_in_at)
        .fetch_optional(db)
        .await
    };
    let (prev, next) = tokio::try_join!(prev, next)?;

    let to_date = |t: i64, fallback: NaiveDateTime| {
        mz_core::time::JsDate::from_millis(t).map_or(fallback, |d| d.0)
    };
    let lead_start = ms(s.clock_in_at) - EVENT_LEAD_MS;
    let prev_end = prev.flatten().map_or(i64::MIN, |out| ms(out) + 1);
    let start = to_date(lead_start.max(prev_end), s.clock_in_at);

    if let Some(out) = s.clock_out_at {
        return Ok((start, out));
    }
    let max_end = ms(s.clock_in_at) + 24 * 3600 * 1000;
    let next_end = next.map_or(i64::MAX, |n| ms(n) - EVENT_LEAD_MS - 1);
    let end = ms(now_naive()).min(max_end).min(next_end);
    Ok((start, to_date(end, s.clock_in_at)))
}

#[derive(sqlx::FromRow)]
struct LocEvent {
    event: String,
    gf_uuid: String,
    gf_name: String,
    lat: f64,
    lng: f64,
    accuracy: f64,
    timestamp: NaiveDateTime,
}

/// Node: `getAdminAttendanceDetail` (:1383).
pub async fn admin_attendance_detail(
    state: &AppState,
    user_id: i32,
    shift_uuid: &str,
) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = mz_core::company::require_company_id(db, user_id).await?;
    let shift_q = async {
        sqlx::query_as::<_, ShiftRow>(&format!(
            r#"SELECT {SHIFT_COLUMNS}{FROM} WHERE s.uuid = $1::uuid AND s."companyId" = $2 LIMIT 1"#
        ))
        .bind(shift_uuid)
        .bind(company_id)
        .fetch_optional(db)
        .await
    };
    let gates = mz_core::plan::company_gates(db, company_id);
    let (shift, gates) = tokio::try_join!(shift_q, gates)?;
    let show_route = gates.is_none_or(|g| g.has(mz_core::plan::PlanFeature::TimelineRoute));
    let Some(s) = shift else {
        return Err(ApiError::not_found("Shift not found"));
    };
    let map = geofence_map(db, std::slice::from_ref(&s)).await?;
    let (start, end) = event_window(db, &s).await?;
    let audit = sqlx::query_as::<_, LocEvent>(
        r#"SELECT l.event, g.uuid::text AS gf_uuid, g.name AS gf_name, l.lat, l.lng, l.accuracy, l.timestamp
             FROM "geofenceAuditLog" l JOIN geofence g ON g.id = l."geofenceId"
            WHERE l."userId" = $1 AND g."companyId" = $2 AND l.timestamp >= $3 AND l.timestamp <= $4
            ORDER BY l.id"#,
    )
    .bind(s.user_id)
    .bind(company_id)
    .bind(start)
    .bind(end)
    .fetch_all(db);
    let visits = async {
        if !show_route {
            // Paket tanpa `timelineRoute`: query dilewati, bukan hasilnya disaring.
            return Ok(Vec::new());
        }
        sqlx::query_as::<_, LocEvent>(
            r#"SELECT e.event, st.uuid::text AS gf_uuid, st.name AS gf_name, e.lat, e.lng, e.accuracy, e.timestamp
                 FROM "storeVisitLocationEvent" e
                 JOIN "storeVisit" v ON v.id = e."visitId"
                 JOIN store st ON st.id = e."storeId"
                WHERE e."userId" = $1 AND v."companyId" = $2 AND e.timestamp >= $3 AND e.timestamp <= $4
                ORDER BY e.id"#,
        )
        .bind(s.user_id)
        .bind(company_id)
        .bind(start)
        .bind(end)
        .fetch_all(db)
        .await
    };
    // Sumber ketiga, `source: 'location'`: ping dari user TANPA geofence (`userLocationPing`).
    // Tidak ada zona, jadi `geofence: null` dan `event: 'exit'` (dashboard memetakan ikon dari
    // enter/exit). Tidak digerbang paket, sama dengan sisi `geofence`.
    let pings = sqlx::query_as::<_, (f64, f64, f64, NaiveDateTime)>(
        r#"SELECT lat, lng, accuracy, timestamp FROM "userLocationPing"
            WHERE "userId" = $1 AND "companyId" = $2 AND timestamp >= $3 AND timestamp <= $4
            ORDER BY id"#,
    )
    .bind(s.user_id)
    .bind(company_id)
    .bind(start)
    .bind(end)
    .fetch_all(db);
    let (audit, visits, pings) = tokio::try_join!(audit, visits, pings)?;
    let mut events: Vec<(i64, Value)> =
        Vec::with_capacity(audit.len() + visits.len() + pings.len());
    for (source, rows) in [("geofence", audit), ("store_visit", visits)] {
        for l in rows {
            events.push((
                ms(l.timestamp),
                json!({
                    "source": source,
                    "event": l.event,
                    "geofence": { "id": l.gf_uuid, "name": l.gf_name },
                    "lat": num(l.lat),
                    "lng": num(l.lng),
                    "accuracy": num(l.accuracy),
                    "timestamp": ms(l.timestamp),
                }),
            ));
        }
    }
    for (lat, lng, accuracy, timestamp) in pings {
        events.push((
            ms(timestamp),
            json!({
                "source": "location",
                "event": "exit",
                "geofence": null,
                "lat": num(lat),
                "lng": num(lng),
                "accuracy": num(accuracy),
                "timestamp": ms(timestamp),
            }),
        ));
    }
    // Sort stabil, sama dengan `Array.prototype.sort` Node.
    events.sort_by_key(|(t, _)| *t);
    Ok(json!({
        "shiftId": s.uuid,
        "user": user_of(&s),
        "clockIn": punch_in(&s, &map, true),
        "clockOut": punch_out(&s, &map, true),
        "durationSeconds": s.duration_seconds,
        "status": admin_status(&s.status),
        "note": s.note,
        "gps": s.gps.admin().to_json(),
        "geofenceEvents": events.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
    }))
}

/// `signSelfieUrl`: gagal tanda tangan → `null` (dicatat).
async fn sign_selfie(state: &AppState, key: String, now_ms: i64) -> (String, Option<String>) {
    let opts = mz_files::SignedObjectOpts {
        content_type: Some(Some("image/jpeg")),
        filename: None,
    };
    match mz_files::get_signed_object_url(state, &key, &opts, now_ms).await {
        Ok(u) => (key, Some(u.url)),
        Err(e) => {
            tracing::warn!("failed to sign selfie URL for {key}: {e}");
            (key, None)
        }
    }
}

/// Node: `getAdminAttendanceSelfie` (:1526).
pub async fn admin_attendance_selfie(
    state: &AppState,
    user_id: i32,
    q: &AttendanceQuery,
) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = mz_core::company::require_company_id(db, user_id).await?;
    let range = range_of(q)?;
    let geofence_id = filter_geofence(db, company_id, q.geofence_uuid.as_deref()).await?;
    let (start, end) = resolved(&range)?;
    let f = Filter {
        company_id,
        own_user: None,
        start,
        end,
        user_uuid: q.user_uuid.as_deref(),
        geofence_id,
        // `gpsOff` hanya berlaku di daftar attendance & ekspor.
        gps_off: false,
    };
    let (shifts, total) = tokio::try_join!(
        list_shifts(db, &f, Some((q.page, q.limit)), None),
        count_shifts(db, &f),
    )?;
    let map = geofence_map(db, &shifts).await?;
    let now = now_naive();
    let now_ms = ms(now);
    let mut keys: Vec<String> = Vec::new();
    for s in &shifts {
        for k in [&s.in_selfie_key, &s.out_selfie_key].into_iter().flatten() {
            if !k.is_empty() && !keys.contains(k) {
                keys.push(k.clone());
            }
        }
    }
    let urls: HashMap<String, Option<String>> =
        futures::future::join_all(keys.into_iter().map(|k| sign_selfie(state, k, now_ms)))
            .await
            .into_iter()
            .collect();
    let url_of = |k: &Option<String>| -> Value {
        k.as_ref()
            .filter(|k| !k.is_empty())
            .and_then(|k| urls.get(k).cloned().flatten())
            .map_or(Value::Null, Value::String)
    };

    let mut data = Vec::with_capacity(shifts.len());
    for s in &shifts {
        let in_gf = s.in_geofence_id.and_then(|id| map.get(&id));
        let out_gf = s.out_geofence_id.and_then(|id| map.get(&id));
        let p = match in_gf {
            Some(g) => schedule::progress(
                s.clock_in_at,
                s.clock_out_at.unwrap_or(now),
                g.weekly_schedule.as_ref(),
                g.timezone.as_deref(),
            )?,
            None => schedule::Progress {
                schedule: None,
                percent: None,
            },
        };
        let clock_out = match s.clock_out_at {
            Some(at) => json!({
                "at": ms(at),
                "selfieUrl": url_of(&s.out_selfie_key),
                "geofence": out_gf.map_or(Value::Null, GfInfo::short),
            }),
            None => Value::Null,
        };
        data.push(json!({
            "shiftId": s.uuid,
            "user": user_of(s),
            "geofence": in_gf.map_or(Value::Null, |g| json!({
                "id": g.uuid,
                "name": g.name,
                "address": g.address,
            })),
            "schedule": p.schedule.map_or(Value::Null, |(st, en)| json!({
                "startTime": st,
                "endTime": en,
            })),
            "clockIn": {
                "at": ms(s.clock_in_at),
                "selfieUrl": url_of(&s.in_selfie_key),
                "geofence": in_gf.map_or(Value::Null, GfInfo::short),
            },
            "clockOut": clock_out,
            "progressPercent": p.percent.map_or(Value::Null, num),
            "durationSeconds": s.duration_seconds,
            "status": admin_status(&s.status),
        }));
    }
    Ok(json!({
        "startDate": q.start_date,
        "endDate": q.end_date,
        "data": data,
        "meta": meta(total, q.page, q.limit),
    }))
}

/// Satu titik clock-in/out untuk ekspor.
#[derive(Debug, Clone)]
pub struct Punch {
    pub at_ms: i64,
    pub lat: Option<f64>,
    pub lng: Option<f64>,
    pub accuracy: Option<f64>,
    pub is_mock: bool,
    pub verification: Option<String>,
    pub geofence_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExportRow {
    pub user_name: String,
    pub username: String,
    pub clock_in: Punch,
    pub clock_out: Option<Punch>,
    pub duration_seconds: Option<i64>,
    pub late_seconds: Option<i64>,
    pub status: String,
    pub note: Option<String>,
    pub gps: AdminGps,
}

#[derive(Debug, Clone)]
pub struct ExportData {
    pub start_date: String,
    pub end_date: String,
    pub summary: Summary,
    pub rows: Vec<ExportRow>,
}

/// Node: `fetchAttendanceExportData` (:1767), maks 10 000 baris.
pub async fn export_data(
    state: &AppState,
    user_id: i32,
    q: &AttendanceQuery,
) -> ApiResult<ExportData> {
    let db = &state.db;
    let company_id = mz_core::company::require_company_id(db, user_id).await?;
    let range = range_of(q)?;
    let geofence_id = filter_geofence(db, company_id, q.geofence_uuid.as_deref()).await?;
    let (start, end) = resolved(&range)?;
    let f = Filter {
        company_id,
        own_user: None,
        start,
        end,
        user_uuid: q.user_uuid.as_deref(),
        geofence_id,
        gps_off: q.gps_off,
    };
    let (shifts, all, gfs) = tokio::try_join!(
        list_shifts(db, &f, None, Some(10_000)),
        summary_shifts(db, company_id, None, start, end),
        active_company_geofences(db, company_id),
    )?;
    let summary = admin_summary(db, company_id, &all, &gfs).await?;
    let map = geofence_map(db, &shifts).await?;
    let name_of = |id: Option<i32>| id.and_then(|i| map.get(&i)).map(|g| g.name.clone());
    let mut rows = Vec::with_capacity(shifts.len());
    for s in shifts {
        let check = admin_check_set(&gfs, s.user_id, s.in_geofence_id);
        let late = schedule::late_seconds(s.clock_in_at, &check)?;
        rows.push(ExportRow {
            clock_in: Punch {
                at_ms: ms(s.clock_in_at),
                lat: s.in_lat,
                lng: s.in_lng,
                accuracy: s.in_accuracy,
                is_mock: s.in_is_mock,
                verification: Some(s.in_verified.clone()),
                geofence_name: name_of(s.in_geofence_id),
            },
            clock_out: s.clock_out_at.map(|at| Punch {
                at_ms: ms(at),
                lat: s.out_lat,
                lng: s.out_lng,
                accuracy: s.out_accuracy,
                is_mock: s.out_is_mock.unwrap_or(false),
                verification: s.out_verified.clone(),
                geofence_name: name_of(s.out_geofence_id),
            }),
            duration_seconds: s.duration_seconds.map(i64::from),
            late_seconds: late,
            status: admin_status(&s.status).to_owned(),
            note: s.note.clone(),
            gps: s.gps.admin(),
            user_name: s.user_name,
            username: s.user_username,
        });
    }
    Ok(ExportData {
        start_date: q.start_date.clone(),
        end_date: q.end_date.clone(),
        summary,
        rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn averages_round_half_up() {
        assert_eq!(average(0, 0), None);
        assert_eq!(average(3, 2), Some(2));
        assert_eq!(average(5, 3), Some(2));
    }

    #[test]
    fn meta_pages() {
        assert_eq!(meta(0, 1, 20)["totalPages"], 0);
        assert_eq!(meta(21, 1, 20)["totalPages"], 2);
        assert_eq!(meta(20, 1, 20)["totalPages"], 1);
    }

    #[test]
    fn gps_format() {
        let never = AdminGps::default();
        assert_eq!(never.status_label(), "-");
        assert_eq!(never.off_summary(), "-");
        let ok = AdminGps {
            status: Some("ok".into()),
            ..AdminGps::default()
        };
        assert_eq!(ok.status_label(), "OK");
        assert_eq!(ok.off_summary(), "No");
        let off = AdminGps {
            status: Some("permission_denied_forever".into()),
            last_disabled_at_ms: Some(1_749_438_300_000), // 2025-06-09T03:05:00Z
            disabled_count: 2,
            ..AdminGps::default()
        };
        assert_eq!(off.status_label(), "Permission Denied (Permanent)");
        assert_eq!(off.off_summary(), "2x, 06-09 03:05");
        let no_time = AdminGps {
            status: Some("weird".into()),
            disabled_count: 1,
            ..AdminGps::default()
        };
        assert_eq!(no_time.status_label(), "weird");
        assert_eq!(no_time.off_summary(), "1x");
        let row = ShiftGps {
            last_disabled_lat: Some(-6.2),
            last_disabled_lng: None,
            ..ShiftGps::default()
        };
        assert_eq!(row.admin().last_disabled_location, None);
        assert_eq!(
            row.admin().to_json(),
            json!({"status": null, "updatedAt": null, "lastDisabledAt": null,
                   "lastDisabledLocation": null, "disabledCount": 0})
        );
    }

    #[test]
    fn status_mapping() {
        assert_eq!(admin_status("auto_closed"), "closed");
        assert_eq!(admin_status("active"), "active");
    }
}
