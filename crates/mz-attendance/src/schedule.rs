//! Jadwal mingguan geofence: `parseTimeAMPM`, `getLocalTimeParts`, `getLateSeconds`,
//! dan `progressPercent` (Node: `src/mobile-clock/mobile-clock.service.ts:895-958, 1656-1700`).
//!
//! Galat yang di Node meledak sebagai `TypeError`/`RangeError` (500): `startTime` bukan string
//! (`.match` pada non-string) dan zona waktu yang ditolak `Intl.DateTimeFormat`.

use chrono::{Datelike, NaiveDateTime, TimeZone, Timelike};
use chrono_tz::Tz;
use mz_core::ApiError;
use serde_json::Value;

/// Truthiness JS untuk nilai JSON.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// `typeof v === 'object' && v !== null` (array juga objek).
pub fn is_js_object(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Object(_) | Value::Array(_)))
}

fn is_js_ws(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

/// `parseTimeAMPM`: `/^(\d{1,2}):(\d{2})\s+(AM|PM)$/`, gagal → `00:00`. Nilai non-string → 500
/// (`timeStr.match` di Node melempar `TypeError`).
pub fn parse_time_ampm(v: &Value) -> Result<(i64, i64), ApiError> {
    let Value::String(s) = v else {
        return Err(ApiError::internal(anyhow::anyhow!(
            "timeStr.match is not a function"
        )));
    };
    Ok(parse_time_str(s).unwrap_or((0, 0)))
}

fn parse_time_str(s: &str) -> Option<(i64, i64)> {
    let (h, rest) = s.split_once(':')?;
    if h.is_empty() || h.len() > 2 || !h.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let m = rest.get(..2)?;
    if !m.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let tail = rest.get(2..)?;
    let trimmed = tail.trim_start_matches(is_js_ws);
    if trimmed.len() == tail.len() {
        return None;
    }
    let ampm = match trimmed {
        "AM" => false,
        "PM" => true,
        _ => return None,
    };
    let mut hours: i64 = h.parse().ok()?;
    let minutes: i64 = m.parse().ok()?;
    if ampm && hours != 12 {
        hours += 12;
    }
    if !ampm && hours == 12 {
        hours = 0;
    }
    Some((hours, minutes))
}

/// Zona untuk `Intl.DateTimeFormat({ timeZone })`: nama IANA tanpa membedakan huruf besar;
/// tidak dikenal → `RangeError` (500).
pub fn intl_zone(name: &str) -> Result<Tz, ApiError> {
    if let Ok(tz) = name.parse::<Tz>() {
        return Ok(tz);
    }
    chrono_tz::TZ_VARIANTS
        .iter()
        .copied()
        .find(|tz| tz.name().eq_ignore_ascii_case(name))
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("Invalid time zone specified: {name}")))
}

/// `getLocalTimeParts(date, timezone)` → (nama hari huruf kecil, jam 0-23, menit).
pub fn local_time_parts(at: NaiveDateTime, tz: Tz) -> (&'static str, i64, i64) {
    let local = tz.from_utc_datetime(&at);
    let day = match local.weekday() {
        chrono::Weekday::Mon => "monday",
        chrono::Weekday::Tue => "tuesday",
        chrono::Weekday::Wed => "wednesday",
        chrono::Weekday::Thu => "thursday",
        chrono::Weekday::Fri => "friday",
        chrono::Weekday::Sat => "saturday",
        chrono::Weekday::Sun => "sunday",
    };
    (day, i64::from(local.hour()), i64::from(local.minute()))
}

/// Jadwal geofence untuk perhitungan telat.
#[derive(Debug, Clone)]
pub struct ScheduleGeofence {
    pub id: i32,
    pub weekly_schedule: Option<Value>,
    pub timezone: Option<String>,
}

/// `getLateSeconds(clockInAt, geofences)`: geofence pertama yang jadwal harinya aktif DAN
/// clock-in-nya lewat jam mulai → detik telat; `None` = tidak telat.
pub fn late_seconds(
    clock_in_at: NaiveDateTime,
    geofences: &[&ScheduleGeofence],
) -> Result<Option<i64>, ApiError> {
    for gf in geofences {
        let ws = gf.weekly_schedule.as_ref();
        if !truthy(ws) || !is_js_object(ws) {
            continue;
        }
        let tz = intl_zone(gf.timezone.as_deref().unwrap_or("UTC"))?;
        let (day, hour, minute) = local_time_parts(clock_in_at, tz);
        let schedule = ws.and_then(|w| w.get(day));
        if !truthy(schedule.and_then(|s| s.get("enabled"))) {
            continue;
        }
        let start = schedule
            .and_then(|s| s.get("startTime"))
            .unwrap_or(&Value::Null);
        let (sh, sm) = parse_time_ampm(start)?;
        let diff = hour * 60 + minute - (sh * 60 + sm);
        if diff > 0 {
            return Ok(Some(diff * 60));
        }
    }
    Ok(None)
}

/// `schedule` + `progressPercent` di daftar selfie admin.
pub struct Progress {
    pub schedule: Option<(Value, Value)>,
    pub percent: Option<f64>,
}

/// Node: `getAdminAttendanceSelfie` :1656-1700.
pub fn progress(
    clock_in_at: NaiveDateTime,
    reference: NaiveDateTime,
    weekly_schedule: Option<&Value>,
    timezone: Option<&str>,
) -> Result<Progress, ApiError> {
    let mut out = Progress {
        schedule: None,
        percent: None,
    };
    if !truthy(weekly_schedule) || !is_js_object(weekly_schedule) {
        return Ok(out);
    }
    let tz = intl_zone(timezone.unwrap_or("UTC"))?;
    let (day, hour, minute) = local_time_parts(clock_in_at, tz);
    let Some(day_schedule) = weekly_schedule.and_then(|w| w.get(day)) else {
        return Ok(out);
    };
    let start = day_schedule.get("startTime");
    let end = day_schedule.get("endTime");
    if !(truthy(day_schedule.get("enabled")) && truthy(start) && truthy(end)) {
        return Ok(out);
    }
    let (Some(start), Some(end)) = (start, end) else {
        return Ok(out);
    };
    out.schedule = Some((start.clone(), end.clone()));
    let (sh, sm) = parse_time_ampm(start)?;
    let (eh, em) = parse_time_ampm(end)?;
    let total_minutes = eh * 60 + em - (sh * 60 + sm);
    if total_minutes > 0 {
        let clock_in_ms = clock_in_at.and_utc().timestamp_millis();
        let clock_in_local = hour * 60 + minute;
        let start_local = sh * 60 + sm;
        let scheduled_start =
            clock_in_ms.div_euclid(60_000) * 60_000 - (clock_in_local - start_local) * 60_000;
        let total_ms = total_minutes * 60_000;
        let reference_ms = reference.and_utc().timestamp_millis();
        let elapsed = (reference_ms.min(scheduled_start + total_ms) - scheduled_start).max(0);
        #[allow(clippy::cast_precision_loss)]
        let ratio = elapsed as f64 / total_ms as f64;
        out.percent = Some(((ratio * 1000.0) + 0.5).floor() / 10.0);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(s: &str) -> NaiveDateTime {
        mz_core::time::parse_js_date(s).unwrap_or_default()
    }

    #[test]
    fn parses_ampm() {
        assert_eq!(parse_time_str("09:00 AM"), Some((9, 0)));
        assert_eq!(parse_time_str("12:30 AM"), Some((0, 30)));
        assert_eq!(parse_time_str("12:05 PM"), Some((12, 5)));
        assert_eq!(parse_time_str("6:15  PM"), Some((18, 15)));
        assert_eq!(parse_time_str("06:00PM"), None);
        assert_eq!(parse_time_str("123:00 PM"), None);
        assert!(parse_time_ampm(&json!(5)).is_err());
        assert_eq!(parse_time_ampm(&json!("x")).ok(), Some((0, 0)));
    }

    #[test]
    fn late_uses_geofence_zone() {
        let ws =
            json!({"monday": {"enabled": true, "startTime": "09:00 AM", "endTime": "06:00 PM"}});
        let gf = ScheduleGeofence {
            id: 1,
            weekly_schedule: Some(ws),
            timezone: Some("Asia/Jakarta".into()),
        };
        // Senin 2026-06-01 09:30 WIB = 02:30Z.
        assert_eq!(
            late_seconds(at("2026-06-01T02:30:00Z"), &[&gf]).ok(),
            Some(Some(1800))
        );
        assert_eq!(
            late_seconds(at("2026-06-01T01:59:00Z"), &[&gf]).ok(),
            Some(None)
        );
        // Selasa: tidak ada jadwal.
        assert_eq!(
            late_seconds(at("2026-06-02T05:00:00Z"), &[&gf]).ok(),
            Some(None)
        );
        let bad = ScheduleGeofence {
            id: 2,
            weekly_schedule: Some(json!({"monday": {"enabled": true}})),
            timezone: Some("UTC".into()),
        };
        assert!(late_seconds(at("2026-06-01T10:00:00Z"), &[&bad]).is_err());
    }

    #[test]
    fn progress_percent() {
        let ws =
            json!({"monday": {"enabled": true, "startTime": "09:00 AM", "endTime": "06:00 PM"}});
        // Mulai 09:00, selesai 18:00 (540 mnt); referensi 13:30 → 270/540 = 50%.
        let p = progress(
            at("2026-06-01T09:10:00Z"),
            at("2026-06-01T13:30:00Z"),
            Some(&ws),
            None,
        )
        .ok();
        let p = p.map(|p| (p.percent, p.schedule.is_some()));
        assert_eq!(p, Some((Some(50.0), true)));
        // Referensi sebelum jam mulai → 0; sesudah jam selesai → 100.
        let early = progress(
            at("2026-06-01T08:00:00Z"),
            at("2026-06-01T08:30:00Z"),
            Some(&ws),
            None,
        )
        .ok()
        .and_then(|p| p.percent);
        assert_eq!(early, Some(0.0));
        let late = progress(
            at("2026-06-01T09:00:00Z"),
            at("2026-06-01T20:00:00Z"),
            Some(&ws),
            Some("utc"),
        )
        .ok()
        .and_then(|p| p.percent);
        assert_eq!(late, Some(100.0));
    }

    #[test]
    fn intl_zone_is_case_insensitive() {
        assert!(intl_zone("asia/jakarta").is_ok());
        assert!(intl_zone("Bogus/X").is_err());
    }
}
