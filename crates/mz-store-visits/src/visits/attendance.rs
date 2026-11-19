//! Port `src/store-visits/store-visit-attendance.service.ts` — buka shift absensi dari check-in
//! kunjungan untuk company ber-`storeVisitCountsAsAttendance`. Best-effort: galat tidak pernah
//! sampai ke pemanggil (kunjungan sudah ter-commit). Idempoten.

use chrono::{Duration, NaiveDate, NaiveDateTime, TimeZone};
use mz_attendance::clock::abandon_previous_day_shift;
use mz_core::time::now_naive;
use mz_core::tz::zone_or_utc;
use sqlx::PgPool;

/// `AttendanceVisitSource`: kolom `storeVisit` asal shift.
#[derive(Debug, Clone, PartialEq)]
pub struct AttendanceVisitSource {
    pub user_id: i32,
    pub company_id: i32,
    pub visit_number: String,
    pub check_in_at: NaiveDateTime,
    pub check_in_client_at: Option<NaiveDateTime>,
    pub check_in_lat: f64,
    pub check_in_lng: f64,
    pub check_in_accuracy: f64,
    pub check_in_is_mock: bool,
    pub check_in_gps_validation: String,
}

/// `moment.tz(date, zone).startOf('day')` / `.endOf('day')` untuk tanggal lokal `date`, sebagai
/// UTC naif. Jam tengah malam yang tak ada (DST) → saat paling awal yang ada.
pub fn local_day_bounds(zone: &str, date: NaiveDate) -> (NaiveDateTime, NaiveDateTime) {
    let tz = zone_or_utc(Some(zone));
    let start_of = |d: NaiveDate| -> NaiveDateTime {
        let midnight = d.and_hms_opt(0, 0, 0).unwrap_or_default();
        tz.from_local_datetime(&midnight)
            .earliest()
            .or_else(|| {
                tz.from_local_datetime(&(midnight + Duration::hours(1)))
                    .earliest()
            })
            .map_or(midnight, |t| t.naive_utc())
    };
    let start = start_of(date);
    let next = date.succ_opt().map_or(start + Duration::days(1), start_of);
    (start, next - Duration::milliseconds(1))
}

/// Tanggal lokal momen UTC `at` di zona `zone`.
pub fn local_date(zone: &str, at: NaiveDateTime) -> NaiveDate {
    zone_or_utc(Some(zone)).from_utc_datetime(&at).date_naive()
}

/// `verificationFor`: mock → `failed`; `valid` → `verified`; selain itu `unverified`.
pub fn verification_for(is_mock: bool, gps_validation: &str) -> &'static str {
    if is_mock {
        "failed"
    } else if gps_validation == "valid" {
        "verified"
    } else {
        "unverified"
    }
}

async fn open_shift_tx(db: &PgPool, visit: &AttendanceVisitSource, zone: &str) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    let (day_start, day_end) = local_day_bounds(zone, local_date(zone, visit.check_in_at));
    let shift_today: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "mobileShift"
            WHERE "userId" = $1 AND "companyId" = $2 AND "clockInAt" >= $3 AND "clockInAt" <= $4
            LIMIT 1"#,
    )
    .bind(visit.user_id)
    .bind(visit.company_id)
    .bind(day_start)
    .bind(day_end)
    .fetch_optional(&mut *tx)
    .await?;
    if shift_today.is_some() {
        return tx.commit().await;
    }
    let active: Option<(i32, NaiveDateTime)> = sqlx::query_as(
        r#"SELECT id, "clockInAt" FROM "mobileShift"
            WHERE "userId" = $1 AND "companyId" = $2 AND status = 'active'
            ORDER BY id LIMIT 1"#,
    )
    .bind(visit.user_id)
    .bind(visit.company_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((shift_id, clock_in_at)) = active {
        if clock_in_at >= day_start {
            return tx.commit().await;
        }
        if !abandon_previous_day_shift(&mut tx, shift_id, now_naive()).await? {
            return tx.commit().await;
        }
    }
    sqlx::query(
        r#"INSERT INTO "mobileShift"
             ("userId", "companyId", "clockInAt", "clockInClientAt", note, status, source,
              "clockInLat", "clockInLng", "clockInAccuracy", "clockInIsMock", "clockInGeofenceId",
              "clockInSelfieKey", "clockInVerified")
           VALUES ($1, $2, $3, $4, $5, 'active', 'mobile', $6, $7, $8, $9, NULL, NULL,
                   $10::"PunchVerifStatus")"#,
    )
    .bind(visit.user_id)
    .bind(visit.company_id)
    .bind(visit.check_in_at)
    .bind(visit.check_in_client_at)
    .bind(format!(
        "Auto clock-in from store visit {}",
        visit.visit_number
    ))
    .bind(visit.check_in_lat)
    .bind(visit.check_in_lng)
    .bind(visit.check_in_accuracy)
    .bind(visit.check_in_is_mock)
    .bind(verification_for(
        visit.check_in_is_mock,
        &visit.check_in_gps_validation,
    ))
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// `openShiftForVisit(visit, timeZone)`: zona kosong → UTC. Galat ditelan (unique violation diam,
/// selain itu `tracing::error!`).
pub async fn open_shift_for_visit(
    db: &PgPool,
    visit: &AttendanceVisitSource,
    time_zone: Option<&str>,
) {
    let zone = time_zone.filter(|z| !z.is_empty()).unwrap_or("UTC");
    if let Err(e) = open_shift_tx(db, visit, zone).await {
        if mz_core::db::is_unique_violation(&e) {
            return;
        }
        tracing::error!(
            "attendance shift for visit {} (user {}) failed to open: {e}",
            visit.visit_number,
            visit.user_id
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f").unwrap()
    }

    /// "bounds 'today' by the company timezone, not by UTC".
    #[test]
    fn day_bounds_in_company_zone() {
        // 2026-08-20 01:00 UTC = 08:00 WIB pada 2026-08-20.
        let at = dt("2026-08-20 01:00:00.000");
        let d = local_date("Asia/Jakarta", at);
        assert_eq!(d.to_string(), "2026-08-20");
        let (s, e) = local_day_bounds("Asia/Jakarta", d);
        assert_eq!(s, dt("2026-08-19 17:00:00.000"));
        assert_eq!(e, dt("2026-08-20 16:59:59.999"));
        // 2026-08-19 20:00 UTC sudah 2026-08-20 di Jakarta.
        assert_eq!(
            local_date("Asia/Jakarta", dt("2026-08-19 20:00:00.000")).to_string(),
            "2026-08-20"
        );
    }

    /// "falls back to UTC days when the company never set a timezone".
    #[test]
    fn utc_fallback() {
        let d = local_date("UTC", dt("2026-08-20 23:00:00.000"));
        let (s, e) = local_day_bounds("UTC", d);
        assert_eq!(s, dt("2026-08-20 00:00:00.000"));
        assert_eq!(e, dt("2026-08-20 23:59:59.999"));
        assert_eq!(local_day_bounds("Not/AZone", d).0, s);
    }

    /// "records a mock location as a failed verification" + "does not upgrade an override".
    #[test]
    fn verification() {
        assert_eq!(verification_for(true, "valid"), "failed");
        assert_eq!(verification_for(false, "valid"), "verified");
        assert_eq!(verification_for(false, "outside"), "unverified");
        assert_eq!(verification_for(false, "mock"), "unverified");
    }
}
