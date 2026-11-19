//! Port `src/divisions/division-time.ts` (+ `reports/report-time.ts`, `reports/report-history.ts`,
//! `manual-time-settings/manual-time-quota.ts` `periodBounds` bagian WEEK) — angka waktu tampilan divisi.
//!
//! Zona waktu: kunci tanggal (`YYYY-MM-DD`) hari ini / awal pekan dihitung di zona company
//! (`moment.tz`; nama tak dikenal = UTC). Pembulatan: durasi timeline `floor(ms / 1000)` dengan
//! batas bawah 0; rata-rata per anggota `Math.round` (setengah ke atas).

use std::collections::HashMap;

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime};
use mz_core::time::{JsDate, timeline_duration_seconds};
use mz_core::tz::zone_or_utc;
use sqlx::PgPool;

use crate::consts::{IDLE_WINDOW_MINUTES, ONLINE_WINDOW_MINUTES};

const DATE_KEY_FORMAT: &str = "%Y-%m-%d";
const MINUTE_MS: i64 = 60_000;
/// `DEFAULT_DATA_HISTORY_MONTHS`.
pub const DEFAULT_DATA_HISTORY_MONTHS: i32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberStatus {
    Online,
    Idle,
    Offline,
}

impl MemberStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Online => "ONLINE",
            Self::Idle => "IDLE",
            Self::Offline => "OFFLINE",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tracked {
    pub total: i64,
    pub billable: i64,
    pub non_billable: i64,
}

/// Tanggal lokal instant `ms` di zona company.
fn local_date(ms: i64, tz: chrono_tz::Tz) -> NaiveDate {
    DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .with_timezone(&tz)
        .date_naive()
}

/// `companyDayAndWeek`: kunci hari ini dan awal pekan (`workweekStartDay`: 0 = Minggu … 6 = Sabtu).
/// Selisih hari memakai `%` bertanda seperti JS (`workweekStartDay` di luar 0..=6 menghasilkan
/// selisih negatif = awal pekan di masa depan, sama dengan Node).
pub fn company_day_and_week(
    now_ms: i64,
    time_zone: Option<&str>,
    workweek_start_day: i32,
) -> (String, String) {
    let tz = zone_or_utc(time_zone);
    let today = local_date(now_ms, tz);
    let weekday = i64::from(today.weekday().num_days_from_sunday());
    let back = (weekday - i64::from(workweek_start_day) + 7) % 7;
    let week_start = today - Duration::days(back);
    (
        today.format(DATE_KEY_FORMAT).to_string(),
        week_start.format(DATE_KEY_FORMAT).to_string(),
    )
}

/// `memberStatus`: ONLINE bila aktivitas terakhir berakhir <= 10 menit lalu, IDLE <= 60 menit,
/// selain itu (atau tanpa aktivitas) OFFLINE. Selisih negatif (jam mendahului server) = ONLINE.
pub fn member_status(last_end_ms: Option<i64>, now_ms: i64) -> MemberStatus {
    let Some(last) = last_end_ms else {
        return MemberStatus::Offline;
    };
    let age = now_ms - last;
    if age <= ONLINE_WINDOW_MINUTES * MINUTE_MS {
        MemberStatus::Online
    } else if age <= IDLE_WINDOW_MINUTES * MINUTE_MS {
        MemberStatus::Idle
    } else {
        MemberStatus::Offline
    }
}

/// Satu baris timeline untuk penjumlahan.
#[derive(Debug, Clone, Copy)]
pub struct TimelineRow {
    pub user_id: i32,
    pub start_at: JsDate,
    pub end_at: JsDate,
    pub start_at_utc: Option<JsDate>,
    pub end_at_utc: Option<JsDate>,
    pub billable: Option<bool>,
    pub project_billable: bool,
}

/// Penjumlahan per user (`sumTrackedSecondsByUser` setelah query): billable = nilai timeline bila
/// terisi, jika tidak mengikuti project.
pub fn accumulate(rows: &[TimelineRow]) -> HashMap<i32, Tracked> {
    let mut out: HashMap<i32, Tracked> = HashMap::new();
    for r in rows {
        let seconds = timeline_duration_seconds(r.start_at, r.end_at, r.start_at_utc, r.end_at_utc);
        let e = out.entry(r.user_id).or_default();
        e.total += seconds;
        if r.billable.unwrap_or(r.project_billable) {
            e.billable += seconds;
        } else {
            e.non_billable += seconds;
        }
    }
    out
}

type TimelineTuple = (
    i32,
    JsDate,
    JsDate,
    Option<JsDate>,
    Option<JsDate>,
    Option<bool>,
    bool,
);

/// `sumTrackedSecondsByUser`: total detik tracked per user pada `tracking.takenDate` di
/// `[from_key, to_key]` (kolom `@db.Date`, kunci `YYYY-MM-DD`). Filter sama dengan
/// `ReportsService.find`: status 1 di timeline dan tracking, project milik company.
pub async fn sum_tracked_seconds_by_user(
    db: &PgPool,
    company_id: i32,
    user_ids: &[i32],
    from_key: &str,
    to_key: &str,
) -> sqlx::Result<HashMap<i32, Tracked>> {
    if user_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<TimelineTuple> = sqlx::query_as(
        r#"SELECT t."userId", tl."startAt", tl."endAt", tl."startAtUtc", tl."endAtUtc",
                  tl.billable, p.billable
           FROM "trackingTimeline" tl
           JOIN tracking t ON t.id = tl."trackingId"
           JOIN project p ON p.id = tl."projectId"
           WHERE tl.status = 1 AND t.status = 1
             AND t."userId" = ANY($1)
             AND t."takenDate" >= $2::date AND t."takenDate" <= $3::date
             AND EXISTS (SELECT 1 FROM "projectCompany" pc
                         WHERE pc."projectId" = p.id AND pc."companyId" = $4)"#,
    )
    .bind(user_ids)
    .bind(from_key)
    .bind(to_key)
    .bind(company_id)
    .fetch_all(db)
    .await?;
    let rows: Vec<TimelineRow> = rows
        .into_iter()
        .map(
            |(user_id, start_at, end_at, start_at_utc, end_at_utc, billable, pb)| TimelineRow {
                user_id,
                start_at,
                end_at,
                start_at_utc,
                end_at_utc,
                billable,
                project_billable: pb,
            },
        )
        .collect();
    Ok(accumulate(&rows))
}

type LatestTuple = (i32, Option<JsDate>, Option<JsDate>, Option<i32>);

/// Timeline live terakhir satu user.
#[derive(Debug, Clone, Copy)]
pub struct LatestActivity {
    pub end_at_utc: Option<JsDate>,
    pub end_at: Option<JsDate>,
    pub project_id: Option<i32>,
}

impl LatestActivity {
    /// `endAtUtc ?? endAt` (aturan yang sama dengan durasi laporan).
    pub fn last_end(&self) -> Option<JsDate> {
        self.end_at_utc.or(self.end_at)
    }
}

/// Geser kunci tanggal `days` hari (`shiftDateKey`, UTC).
pub fn shift_date_key(key: &str, days: i64) -> Option<String> {
    let d = NaiveDate::parse_from_str(key, DATE_KEY_FORMAT).ok()?;
    Some(
        (d + Duration::days(days))
            .format(DATE_KEY_FORMAT)
            .to_string(),
    )
}

/// `latestActivityByUser`: timeline live terakhir per user ("sedang tracking" =
/// `MonitoringReportService.countActivity`: status 1 dan bukan manual time), `takenDate` hari
/// ini +-1 hari, dipagari proyek milik company.
pub async fn latest_activity_by_user(
    db: &PgPool,
    company_id: i32,
    user_ids: &[i32],
    today_key: &str,
) -> sqlx::Result<HashMap<i32, LatestActivity>> {
    let mut out = HashMap::new();
    if user_ids.is_empty() {
        return Ok(out);
    }
    let from = shift_date_key(today_key, -1).unwrap_or_else(|| today_key.to_owned());
    let to = shift_date_key(today_key, 1).unwrap_or_else(|| today_key.to_owned());
    let rows: Vec<LatestTuple> = sqlx::query_as(
        r#"SELECT DISTINCT ON (t."userId")
                  t."userId", tl."endAtUtc", tl."endAt", tl."projectId"
           FROM   "tracking" t
           JOIN   "trackingTimeline" tl ON tl."trackingId" = t."id"
           WHERE  t."userId" = ANY($1::int[])
             AND  t."takenDate" BETWEEN $2::date AND $3::date
             AND  tl."status" = 1
             AND  tl."manualTimeStatus" IS NULL
             AND  EXISTS (
                    SELECT 1 FROM "projectCompany" pc
                    WHERE  pc."projectId" = tl."projectId"
                      AND  pc."companyId" = $4
                  )
           ORDER  BY t."userId", COALESCE(tl."endAtUtc", tl."endAt") DESC"#,
    )
    .bind(user_ids)
    .bind(from)
    .bind(to)
    .bind(company_id)
    .fetch_all(db)
    .await?;
    for (user_id, end_at_utc, end_at, project_id) in rows {
        out.insert(
            user_id,
            LatestActivity {
                end_at_utc,
                end_at,
                project_id,
            },
        );
    }
    Ok(out)
}

// ───────────────────────── batas riwayat (`report-history.ts`) ─────────────────────────

/// `date.setMonth(date.getMonth() + delta)` (proses Node berzona UTC): hari-dalam-bulan
/// dipertahankan dan meluap ke bulan berikutnya (31 Mar - 1 bulan = 3 Mar).
fn js_add_months(d: NaiveDateTime, delta: i64) -> NaiveDateTime {
    let total = i64::from(d.year()) * 12 + i64::from(d.month0()) + delta;
    let (year, month0) = (total.div_euclid(12), total.rem_euclid(12));
    let first = i32::try_from(year)
        .ok()
        .zip(u32::try_from(month0 + 1).ok())
        .and_then(|(y, m)| NaiveDate::from_ymd_opt(y, m, 1));
    match first {
        Some(first) => (first + Duration::days(i64::from(d.day()) - 1)).and_time(d.time()),
        None => d,
    }
}

/// `historyMinAllowedDate`: `null`/0 → 3 bulan; negatif = tanpa batas (`None`).
pub fn history_min_allowed(months: Option<i32>, now_ms: i64) -> Option<NaiveDateTime> {
    let months = match months {
        Some(m) if m != 0 => m,
        _ => DEFAULT_DATA_HISTORY_MONTHS,
    };
    if months < 0 {
        return None;
    }
    let now = DateTime::from_timestamp_millis(now_ms)
        .unwrap_or_default()
        .naive_utc();
    Some(js_add_months(now, -i64::from(months)))
}

/// `clampToHistory`: `(startDate, clamped)`.
pub fn clamp_to_history(
    start: NaiveDateTime,
    months: Option<i32>,
    now_ms: i64,
) -> (NaiveDateTime, bool) {
    match history_min_allowed(months, now_ms) {
        Some(min) if start < min => (min, true),
        _ => (start, false),
    }
}

/// `date.toISOString().split('T')[0]`.
pub fn utc_date_key(d: NaiveDateTime) -> String {
    d.format(DATE_KEY_FORMAT).to_string()
}

/// `Math.round(total / n)` (setengah ke atas) untuk `n > 0`.
pub fn round_average(total: i64, n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    (2 * total + n).div_euclid(2 * n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(s: &str) -> i64 {
        DateTime::parse_from_rfc3339(s)
            .map(|d| d.timestamp_millis())
            .unwrap_or_default()
    }

    fn js(s: &str) -> JsDate {
        JsDate(
            DateTime::parse_from_rfc3339(s)
                .map(|d| d.naive_utc())
                .unwrap_or_default(),
        )
    }

    #[test]
    fn day_and_week_follow_company_zone() {
        // 2026-11-16 (Senin) 18:00 UTC = Selasa 17 Nov 01:00 WIB.
        let now = ms("2026-11-16T18:00:00Z");
        assert_eq!(
            company_day_and_week(now, Some("Asia/Jakarta"), 1),
            ("2026-11-17".into(), "2026-11-16".into())
        );
        assert_eq!(
            company_day_and_week(now, Some("UTC"), 1),
            ("2026-11-16".into(), "2026-11-16".into())
        );
        // Pekan mulai Minggu: Selasa 17 Nov → Minggu 15 Nov.
        assert_eq!(
            company_day_and_week(now, Some("Asia/Jakarta"), 0).1,
            "2026-11-15"
        );
        // Pekan mulai Selasa saat hari ini Selasa: awal pekan = hari ini.
        assert_eq!(
            company_day_and_week(now, Some("Asia/Jakarta"), 2).1,
            "2026-11-17"
        );
        // Di belakang UTC: 02:00 UTC Senin = Minggu 15 Nov 21:00 di New York (UTC-5).
        assert_eq!(
            company_day_and_week(ms("2026-11-16T02:00:00Z"), Some("America/New_York"), 1),
            ("2026-11-15".into(), "2026-11-09".into())
        );
    }

    #[test]
    fn day_and_week_unknown_or_blank_zone_is_utc() {
        let now = ms("2026-11-16T23:30:00Z");
        for z in [None, Some(""), Some("Not/AZone")] {
            assert_eq!(
                company_day_and_week(now, z, 1),
                ("2026-11-16".into(), "2026-11-16".into()),
                "{z:?}"
            );
        }
        // Nama zona gaya moment (`asia_jakarta`) dikenali.
        assert_eq!(
            company_day_and_week(now, Some("asia_jakarta"), 1).0,
            "2026-11-17"
        );
    }

    #[test]
    fn day_and_week_across_dst_and_year_boundaries() {
        // DST AS berakhir 1 Nov 2026 (Minggu): 06:30 UTC 1 Nov = 01:30 EST / 02:30 EDT sebelumnya.
        assert_eq!(
            company_day_and_week(ms("2026-11-01T05:30:00Z"), Some("America/New_York"), 1),
            ("2026-11-01".into(), "2026-10-26".into())
        );
        // Pergantian tahun: Kamis 1 Jan 2026, awal pekan Senin = 29 Des 2025.
        assert_eq!(
            company_day_and_week(ms("2026-01-01T10:00:00Z"), Some("UTC"), 1),
            ("2026-01-01".into(), "2025-12-29".into())
        );
        // Zona di depan UTC melewati tengah malam.
        assert_eq!(
            company_day_and_week(ms("2025-12-31T17:00:00Z"), Some("Pacific/Auckland"), 1),
            ("2026-01-01".into(), "2025-12-29".into())
        );
    }

    /// Keluaran Node (moment-timezone Mudah-API, `TZ=UTC`, 2026-11-17): `periodBounds` WEEK +
    /// `format('YYYY-MM-DD')`. Termasuk zona dengan DST tepat tengah malam dan offset 30 menit.
    #[test]
    fn day_and_week_golden_from_node() {
        const CASES: &[(&str, &str, i32, &str, &str)] = &[
            (
                "2026-11-16T18:00:00Z",
                "Asia/Jakarta",
                1,
                "2026-11-17",
                "2026-11-16",
            ),
            (
                "2026-11-16T18:00:00Z",
                "Asia/Jakarta",
                0,
                "2026-11-17",
                "2026-11-15",
            ),
            (
                "2026-11-16T18:00:00Z",
                "Asia/Jakarta",
                2,
                "2026-11-17",
                "2026-11-17",
            ),
            (
                "2026-11-16T02:00:00Z",
                "America/New_York",
                1,
                "2026-11-15",
                "2026-11-09",
            ),
            (
                "2026-11-01T05:30:00Z",
                "America/New_York",
                1,
                "2026-11-01",
                "2026-10-26",
            ),
            (
                "2025-12-31T17:00:00Z",
                "Pacific/Auckland",
                1,
                "2026-01-01",
                "2025-12-29",
            ),
            ("2026-11-15T12:00:00Z", "UTC", 8, "2026-11-15", "2026-11-16"),
            ("2026-11-18T12:00:00Z", "UTC", 8, "2026-11-18", "2026-11-16"),
            (
                "2026-11-16T23:30:00Z",
                "Not/AZone",
                1,
                "2026-11-16",
                "2026-11-16",
            ),
            (
                "2026-11-16T23:30:00Z",
                "asia_jakarta",
                1,
                "2026-11-17",
                "2026-11-16",
            ),
            (
                "2018-11-04T03:30:00Z",
                "America/Sao_Paulo",
                1,
                "2018-11-04",
                "2018-10-29",
            ),
            (
                "2018-11-04T12:00:00Z",
                "America/Sao_Paulo",
                0,
                "2018-11-04",
                "2018-11-04",
            ),
            (
                "2026-03-29T00:30:00Z",
                "Asia/Beirut",
                1,
                "2026-03-29",
                "2026-03-23",
            ),
            (
                "2026-03-28T22:30:00Z",
                "America/Havana",
                0,
                "2026-03-28",
                "2026-03-22",
            ),
            (
                "2026-03-08T10:00:00Z",
                "America/Havana",
                1,
                "2026-03-08",
                "2026-03-02",
            ),
            (
                "2026-10-04T14:00:00Z",
                "Australia/Lord_Howe",
                0,
                "2026-10-05",
                "2026-10-04",
            ),
        ];
        for (t, z, w, today, week) in CASES {
            assert_eq!(
                company_day_and_week(ms(t), Some(z), *w),
                ((*today).to_owned(), (*week).to_owned()),
                "{t} {z} {w}"
            );
        }
    }

    /// `setMonth` Node: 29 Feb 2024 - 12 bulan = "29 Feb 2023" → 1 Mar 2023.
    #[test]
    fn history_golden_from_node() {
        for (now, months, want) in [
            ("2026-11-16T10:00:00Z", None, Some("2026-08-16")),
            ("2026-11-16T10:00:00Z", Some(0), Some("2026-08-16")),
            ("2026-05-31T12:00:00Z", Some(3), Some("2026-03-03")),
            ("2026-03-30T12:00:00Z", Some(1), Some("2026-03-02")),
            ("2026-02-10T00:00:00Z", Some(3), Some("2025-11-10")),
            ("2026-11-16T10:00:00Z", Some(12), Some("2025-11-16")),
            ("2024-02-29T10:00:00Z", Some(12), Some("2023-03-01")),
            ("2026-11-16T10:00:00Z", Some(-1), None),
        ] {
            assert_eq!(
                history_min_allowed(months, ms(now))
                    .map(utc_date_key)
                    .as_deref(),
                want,
                "{now} {months:?}"
            );
        }
    }

    #[test]
    fn workweek_start_out_of_range_matches_js_remainder() {
        // JS: (0 - 8 + 7) % 7 = -1 → awal pekan = besok. 15 Nov 2026 = Minggu.
        let now = ms("2026-11-15T12:00:00Z");
        assert_eq!(company_day_and_week(now, Some("UTC"), 8).1, "2026-11-16");
        // (3 - 8 + 7) % 7 = 2.
        assert_eq!(
            company_day_and_week(ms("2026-11-18T12:00:00Z"), Some("UTC"), 8).1,
            "2026-11-16"
        );
    }

    #[test]
    fn status_windows_are_inclusive() {
        let now = 1_000_000_000_i64;
        assert_eq!(member_status(None, now), MemberStatus::Offline);
        assert_eq!(
            member_status(Some(now - 10 * 60_000), now),
            MemberStatus::Online
        );
        assert_eq!(
            member_status(Some(now - 10 * 60_000 - 1), now),
            MemberStatus::Idle
        );
        assert_eq!(
            member_status(Some(now - 60 * 60_000), now),
            MemberStatus::Idle
        );
        assert_eq!(
            member_status(Some(now - 60 * 60_000 - 1), now),
            MemberStatus::Offline
        );
        assert_eq!(member_status(Some(now + 5_000), now), MemberStatus::Online);
    }

    #[test]
    fn duration_prefers_utc_floors_and_clamps() {
        let a = js("2026-11-16T08:00:00.000Z");
        let b = js("2026-11-16T08:00:01.999Z");
        assert_eq!(timeline_duration_seconds(a, b, None, None), 1);
        // Kolom UTC menang atas kolom lokal.
        assert_eq!(
            timeline_duration_seconds(
                a,
                js("2026-11-16T09:00:00Z"),
                Some(js("2026-11-16T01:00:00Z")),
                Some(js("2026-11-16T01:10:00Z"))
            ),
            600
        );
        // Hanya satu sisi UTC: sisi lain jatuh ke kolom lokal.
        assert_eq!(
            timeline_duration_seconds(a, b, Some(js("2026-11-16T07:59:59.000Z")), None),
            2
        );
        // Terbalik → 0, bukan negatif.
        assert_eq!(timeline_duration_seconds(b, a, None, None), 0);
        assert_eq!(
            timeline_duration_seconds(
                js("2026-11-16T08:00:00.000Z"),
                js("2026-11-16T08:00:00.999Z"),
                None,
                None
            ),
            0
        );
    }

    #[test]
    fn accumulate_splits_billable_with_project_fallback() {
        let t = |user, bill, pb, start: &str, end: &str| TimelineRow {
            user_id: user,
            start_at: js(start),
            end_at: js(end),
            start_at_utc: None,
            end_at_utc: None,
            billable: bill,
            project_billable: pb,
        };
        let rows = [
            t(
                1,
                None,
                true,
                "2026-11-16T08:00:00Z",
                "2026-11-16T09:00:00Z",
            ),
            t(
                1,
                Some(false),
                true,
                "2026-11-16T09:00:00Z",
                "2026-11-16T09:30:00Z",
            ),
            t(
                1,
                None,
                false,
                "2026-11-16T10:00:00Z",
                "2026-11-16T10:10:00Z",
            ),
            t(
                2,
                Some(true),
                false,
                "2026-11-16T08:00:00Z",
                "2026-11-16T08:01:00Z",
            ),
        ];
        let m = accumulate(&rows);
        assert_eq!(
            m.get(&1),
            Some(&Tracked {
                total: 3600 + 1800 + 600,
                billable: 3600,
                non_billable: 1800 + 600
            })
        );
        assert_eq!(
            m.get(&2),
            Some(&Tracked {
                total: 60,
                billable: 60,
                non_billable: 0
            })
        );
        assert_eq!(m.get(&3), None);
    }

    #[test]
    fn history_limit_semantics() {
        let now = ms("2026-11-16T10:00:00.000Z");
        // null / 0 → 3 bulan; negatif → tanpa batas.
        let three = history_min_allowed(None, now);
        assert_eq!(three, history_min_allowed(Some(0), now));
        assert_eq!(three.map(utc_date_key).as_deref(), Some("2026-08-16"));
        assert_eq!(history_min_allowed(Some(-1), now), None);
        assert_eq!(
            history_min_allowed(Some(12), now)
                .map(utc_date_key)
                .as_deref(),
            Some("2025-11-16")
        );
        // Melewati batas tahun.
        assert_eq!(
            history_min_allowed(Some(3), ms("2026-02-10T00:00:00Z"))
                .map(utc_date_key)
                .as_deref(),
            Some("2025-11-10")
        );
    }

    #[test]
    fn history_limit_overflows_day_like_set_month() {
        // 31 Mei - 3 bulan = "31 Feb" → 3 Mar.
        assert_eq!(
            history_min_allowed(Some(3), ms("2026-05-31T12:00:00Z"))
                .map(utc_date_key)
                .as_deref(),
            Some("2026-03-03")
        );
        // 30 Mar - 1 bulan = "30 Feb 2026" → 2 Mar.
        assert_eq!(
            history_min_allowed(Some(1), ms("2026-03-30T12:00:00Z"))
                .map(utc_date_key)
                .as_deref(),
            Some("2026-03-02")
        );
    }

    #[test]
    fn clamp_reports_whether_moved() {
        let now = ms("2026-11-16T10:00:00.000Z");
        let old = js("2026-01-01T00:00:00Z").0;
        let (s, clamped) = clamp_to_history(old, Some(3), now);
        assert!(clamped);
        assert_eq!(utc_date_key(s), "2026-08-16");
        // Persis di batas tidak digeser (`startDate < min`).
        let edge = js("2026-08-16T10:00:00.000Z").0;
        assert_eq!(clamp_to_history(edge, Some(3), now), (edge, false));
        // Tanpa batas.
        assert_eq!(clamp_to_history(old, Some(-1), now), (old, false));
    }

    #[test]
    fn average_rounds_half_up_like_math_round() {
        assert_eq!(round_average(0, 3), 0);
        assert_eq!(round_average(5, 2), 3); // 2.5 → 3
        assert_eq!(round_average(7, 3), 2); // 2.33 → 2
        assert_eq!(round_average(8, 3), 3); // 2.67 → 3
        assert_eq!(round_average(1, 2), 1); // 0.5 → 1
        assert_eq!(round_average(10, 4), 3); // 2.5 → 3
        assert_eq!(round_average(100, 0), 0);
    }

    #[test]
    fn shift_date_key_crosses_month_and_year() {
        assert_eq!(
            shift_date_key("2026-01-01", -1).as_deref(),
            Some("2025-12-31")
        );
        assert_eq!(
            shift_date_key("2026-02-28", 1).as_deref(),
            Some("2026-03-01")
        );
        assert_eq!(shift_date_key("bad", 1), None);
    }
}
