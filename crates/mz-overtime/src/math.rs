//! Hitungan murni lembur PP 35 — port `src/overtime/overtime-math.ts`. Semua dalam detik;
//! menit hanya untuk tampilan (`floor(detik / 60)`).

use chrono::{Datelike, Duration, NaiveDate, NaiveTime};
use serde::Serialize;

use crate::calendar::{DayType, WEEKLY_LIMIT_SEC};
use crate::zone::{parse_date_key, wall_to_ms, zone_or_utc};

/// `RangeError` Node (input tidak valid).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeError(pub String);

impl std::fmt::Display for RangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RangeError {}

/// FR-025: `clamp(worked − normal, 0, approved)`. Detik, tanpa pembulatan.
pub fn overtime_sec_for(worked_sec: i64, normal_sec: i64, approved_sec: i64) -> i64 {
    0.max((worked_sec - normal_sec).min(approved_sec))
}

/// Jam untuk pesan: dari menit utuh, maks. 2 desimal, koma desimal, tanpa ",0".
pub fn format_hours(sec: i64) -> String {
    let hours = sec.div_euclid(60) as f64 / 60.0;
    // `Math.round` JS: setengah dibulatkan ke +∞.
    let rounded = (hours * 100.0 + 0.5).floor() / 100.0;
    format!("{rounded}").replace('.', ",")
}

/// FX-7.3: lembur + jam normal hari itu tak boleh melebihi 24 jam.
pub const DAY_SEC: i64 = 24 * 3600;

pub fn exceeds_day(requested_sec: i64, normal_sec: i64) -> bool {
    requested_sec + normal_sec > DAY_SEC
}

pub fn duration_exceeds_day_message(requested_sec: i64, normal_sec: i64) -> String {
    format!(
        "Lembur {} jam + jam normal {} jam melebihi 24 jam.",
        format_hours(requested_sec),
        format_hours(normal_sec)
    )
}

fn to_minutes(sec: i64) -> i64 {
    sec.div_euclid(60)
}

#[derive(Debug, Clone)]
pub struct QuotaInput {
    pub date: String,
    pub day_type: DayType,
    pub pattern_work_days: i32,
    /// Jam normal tanggal itu (sudah dipotong cuti setengah hari bila ada).
    pub normal_sec: i64,
    pub daily_limit_sec: i64,
    pub requested_sec: i64,
    /// Σ overtimeSec COMPLETED WORKDAY di minggu itu; diabaikan untuk HOLIDAY.
    pub weekly_used_sec: i64,
    pub weekly_pending_sec: Option<i64>,
    pub weekly_limit_sec: Option<i64>,
}

/// Bentuk respons API (`quotaView`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaEvaluation {
    pub date: String,
    pub day_type: DayType,
    pub pattern_work_days: i32,
    pub normal_minutes: i64,
    pub daily_limit_minutes: i64,
    pub requested_minutes: i64,
    pub daily_remaining_minutes: i64,
    pub weekly_limit_minutes: Option<i64>,
    pub weekly_used_minutes: Option<i64>,
    pub weekly_pending_minutes: Option<i64>,
    pub weekly_remaining_minutes: Option<i64>,
    pub exceeds_daily: bool,
    pub exceeds_weekly: bool,
    pub messages: Vec<String>,
}

/// Nilai detik internal (research R7), tidak ikut dikirim ke klien.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaSeconds {
    pub normal_sec: i64,
    pub daily_limit_sec: i64,
    pub requested_sec: i64,
    pub daily_remaining_sec: i64,
    pub weekly_limit_sec: Option<i64>,
    pub weekly_used_sec: Option<i64>,
    pub weekly_remaining_sec: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaEvaluationWithSec {
    pub view: QuotaEvaluation,
    pub sec: QuotaSeconds,
}

/// R7 `evaluateQuota`: harian = requested vs dailyLimit; sisa mingguan =
/// max(0, 18 jam − terpakai). HOLIDAY melewati mingguan. Hanya peringatan.
pub fn evaluate_quota(input: &QuotaInput) -> QuotaEvaluationWithSec {
    let requested_sec = input.requested_sec;
    let daily_limit_sec = input.daily_limit_sec;
    let is_workday = input.day_type == DayType::Workday;

    let daily_remaining_sec = daily_limit_sec;
    let weekly_limit_sec = is_workday.then(|| input.weekly_limit_sec.unwrap_or(WEEKLY_LIMIT_SEC));
    let weekly_used_sec = is_workday.then_some(input.weekly_used_sec);
    let weekly_remaining_sec =
        weekly_limit_sec.map(|limit| 0.max(limit - weekly_used_sec.unwrap_or(0)));

    let exceeds_daily = requested_sec > daily_remaining_sec;
    let exceeds_weekly = weekly_remaining_sec.is_some_and(|r| requested_sec > r);

    let requested = format_hours(requested_sec);
    let mut messages = Vec::new();
    if exceeds_daily {
        messages.push(format!(
            "Lembur {requested} jam melebihi batas harian {} jam.",
            format_hours(daily_limit_sec)
        ));
    }
    if exceeds_weekly && let Some(limit) = weekly_limit_sec {
        messages.push(format!(
            "Lembur {requested} jam ini melebihi sisa kuota minggu ini (sisa {} jam dari {} jam).",
            format_hours(weekly_remaining_sec.unwrap_or(0)),
            format_hours(limit)
        ));
    }

    QuotaEvaluationWithSec {
        view: QuotaEvaluation {
            date: input.date.clone(),
            day_type: input.day_type,
            pattern_work_days: input.pattern_work_days,
            normal_minutes: to_minutes(input.normal_sec),
            daily_limit_minutes: to_minutes(daily_limit_sec),
            requested_minutes: to_minutes(requested_sec),
            daily_remaining_minutes: to_minutes(daily_remaining_sec),
            weekly_limit_minutes: weekly_limit_sec.map(to_minutes),
            weekly_used_minutes: weekly_used_sec.map(to_minutes),
            weekly_pending_minutes: is_workday
                .then(|| to_minutes(input.weekly_pending_sec.unwrap_or(0))),
            weekly_remaining_minutes: weekly_remaining_sec.map(to_minutes),
            exceeds_daily,
            exceeds_weekly,
            messages,
        },
        sec: QuotaSeconds {
            normal_sec: input.normal_sec,
            daily_limit_sec,
            requested_sec,
            daily_remaining_sec,
            weekly_limit_sec,
            weekly_used_sec,
            weekly_remaining_sec,
        },
    }
}

#[derive(Debug, Clone)]
pub struct WeekRow {
    pub id: i32,
    pub date: NaiveDate,
    /// `None` = kolom `dayType` kosong (tidak dihitung sebagai hari kerja).
    pub day_type: Option<DayType>,
    pub overtime_sec: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WeekRowFlag {
    pub id: i32,
    pub exceeds_weekly_limit: bool,
}

/// R7/FR-032 `relabelWeek`: urut (date, id); jumlah berjalan overtimeSec WORKDAY. Baris
/// pertama yang mendorong total > batas dan semua WORKDAY sesudahnya ditandai.
pub fn relabel_week(rows: &[WeekRow], weekly_limit_sec: i64) -> Vec<WeekRowFlag> {
    let mut sorted: Vec<&WeekRow> = rows.iter().collect();
    sorted.sort_by(|a, b| a.date.cmp(&b.date).then(a.id.cmp(&b.id)));
    let mut running = 0i64;
    let mut exceeded = false;
    sorted
        .into_iter()
        .map(|row| {
            if row.day_type != Some(DayType::Workday) {
                return WeekRowFlag {
                    id: row.id,
                    exceeds_weekly_limit: false,
                };
            }
            running += row.overtime_sec.unwrap_or(0);
            if running > weekly_limit_sec {
                exceeded = true;
            }
            WeekRowFlag {
                id: row.id,
                exceeds_weekly_limit: exceeded,
            }
        })
        .collect()
}

fn assert_week_start_day(workweek_start_day: i32) -> Result<(), RangeError> {
    if (0..=6).contains(&workweek_start_day) {
        Ok(())
    } else {
        Err(RangeError(format!(
            "workweekStartDay harus 0–6: {workweek_start_day}"
        )))
    }
}

fn js_day(d: NaiveDate) -> i32 {
    d.weekday().num_days_from_sunday() as i32
}

fn parse_key(date_key: &str) -> Result<NaiveDate, RangeError> {
    parse_date_key(date_key).ok_or_else(|| RangeError(format!("dateKey tidak valid: {date_key}")))
}

/// Awal minggu `date`; `workweekStartDay` 0 = Minggu … 6 = Sabtu (konvensi company).
pub fn week_start_of(date: NaiveDate, workweek_start_day: i32) -> Result<NaiveDate, RangeError> {
    assert_week_start_day(workweek_start_day)?;
    let days_back = (js_day(date) - workweek_start_day + 7) % 7;
    Ok(date - Duration::days(i64::from(days_back)))
}

/// Versi kunci string (`weekStartOf(dateKey, workweekStartDay)`).
pub fn week_start_of_key(date_key: &str, workweek_start_day: i32) -> Result<String, RangeError> {
    assert_week_start_day(workweek_start_day)?;
    let d = parse_key(date_key)?;
    week_start_of(d, workweek_start_day).map(crate::zone::date_key)
}

/// Rentang minggu inklusif `[start, end]` (7 hari).
pub fn week_range_of(
    date: NaiveDate,
    workweek_start_day: i32,
) -> Result<(NaiveDate, NaiveDate), RangeError> {
    let start = week_start_of(date, workweek_start_day)?;
    Ok((start, start + Duration::days(6)))
}

/// R5 `endOfLocalDayInstant`: 23:59:59.999 lokal pada `dateKey + plusDays` (epoch ms).
/// Zona tidak dikenal → UTC.
pub fn end_of_local_day_instant(
    date_key: &str,
    plus_days: i64,
    time_zone: Option<&str>,
) -> Result<i64, RangeError> {
    let d = parse_key(date_key)? + Duration::days(plus_days);
    let end = NaiveTime::from_hms_milli_opt(23, 59, 59, 999).unwrap_or(NaiveTime::MIN);
    Ok(wall_to_ms(d.and_time(end), zone_or_utc(time_zone)))
}

/// R2 `nextWeekStart`: awal minggu pertama SETELAH `localToday` (hari ini awal minggu → +7).
pub fn next_week_start(
    local_today: NaiveDate,
    workweek_start_day: i32,
) -> Result<NaiveDate, RangeError> {
    assert_week_start_day(workweek_start_day)?;
    let ahead = match (workweek_start_day - js_day(local_today) + 7) % 7 {
        0 => 7,
        n => n,
    };
    Ok(local_today + Duration::days(i64::from(ahead)))
}

#[cfg(test)]
mod tests {
    //! Kasus dari `overtime-math.spec.ts`.
    use super::*;

    const H: i64 = 3_600;
    const M: i64 = 60;

    fn d(s: &str) -> NaiveDate {
        parse_date_key(s).unwrap_or_default()
    }

    #[test]
    fn overtime_sec_for_cases() {
        let cases = [
            (11 * H, 8 * H, 2 * H, 7_200),
            (9 * H + 30 * M, 8 * H, 2 * H, 5_400),
            (7 * H, 8 * H, 2 * H, 0),
            (5 * H, 0, 2 * H, 7_200),
            (8 * H, 7 * H, 2 * H, 3_600),
            (8 * H, 5 * H, 2 * H, 7_200),
            (5 * H + 15 * M, 4 * H, 2 * H, 4_500),
            (0, 8 * H, 2 * H, 0),
            (8 * H + 61, 8 * H, 2 * H, 61),
        ];
        for (worked, normal, approved, expected) in cases {
            assert_eq!(overtime_sec_for(worked, normal, approved), expected);
        }
    }

    #[test]
    fn format_hours_cases() {
        assert_eq!(format_hours(3 * H), "3");
        assert_eq!(format_hours(18 * H), "18");
        assert_eq!(format_hours(90 * M), "1,5");
        assert_eq!(format_hours(100 * M), "1,67");
        assert_eq!(format_hours(0), "0");
    }

    fn workday() -> QuotaInput {
        QuotaInput {
            date: "2026-10-22".into(),
            day_type: DayType::Workday,
            pattern_work_days: 5,
            normal_sec: 8 * H,
            daily_limit_sec: 4 * H,
            requested_sec: 2 * H,
            weekly_used_sec: 0,
            weekly_pending_sec: None,
            weekly_limit_sec: None,
        }
    }

    #[test]
    fn quota_full_week() {
        let e = evaluate_quota(&workday());
        let v = serde_json::to_value(&e.view).unwrap_or_default();
        assert_eq!(
            v,
            serde_json::json!({
                "date": "2026-10-22", "dayType": "WORKDAY", "patternWorkDays": 5,
                "normalMinutes": 480, "dailyLimitMinutes": 240, "requestedMinutes": 120,
                "dailyRemainingMinutes": 240, "weeklyLimitMinutes": 1080, "weeklyUsedMinutes": 0,
                "weeklyPendingMinutes": 0, "weeklyRemainingMinutes": 1080,
                "exceedsDaily": false, "exceedsWeekly": false, "messages": []
            })
        );
        assert_eq!(
            e.sec,
            QuotaSeconds {
                normal_sec: 28_800,
                daily_limit_sec: 14_400,
                requested_sec: 7_200,
                daily_remaining_sec: 14_400,
                weekly_limit_sec: Some(64_800),
                weekly_used_sec: Some(0),
                weekly_remaining_sec: Some(64_800),
            }
        );
    }

    #[test]
    fn quota_weekly_and_daily_messages() {
        let e = evaluate_quota(&QuotaInput {
            requested_sec: 3 * H,
            weekly_used_sec: 16 * H,
            ..workday()
        });
        assert!(e.view.exceeds_weekly && !e.view.exceeds_daily);
        assert_eq!(e.view.weekly_used_minutes, Some(960));
        assert_eq!(e.view.weekly_remaining_minutes, Some(120));
        assert_eq!(
            e.view.messages,
            ["Lembur 3 jam ini melebihi sisa kuota minggu ini (sisa 2 jam dari 18 jam)."]
        );

        let e = evaluate_quota(&QuotaInput {
            requested_sec: 2 * H,
            weekly_used_sec: 16 * H,
            ..workday()
        });
        assert!(!e.view.exceeds_weekly && e.view.messages.is_empty());

        let e = evaluate_quota(&QuotaInput {
            requested_sec: H,
            weekly_used_sec: 20 * H,
            ..workday()
        });
        assert_eq!(e.sec.weekly_remaining_sec, Some(0));
        assert_eq!(
            e.view.messages,
            ["Lembur 1 jam ini melebihi sisa kuota minggu ini (sisa 0 jam dari 18 jam)."]
        );

        let e = evaluate_quota(&QuotaInput {
            requested_sec: 5 * H,
            ..workday()
        });
        assert_eq!(
            e.view.messages,
            ["Lembur 5 jam melebihi batas harian 4 jam."]
        );

        let e = evaluate_quota(&QuotaInput {
            requested_sec: 90 * M + 4 * H,
            weekly_used_sec: 17 * H,
            ..workday()
        });
        assert_eq!(
            e.view.messages,
            [
                "Lembur 5,5 jam melebihi batas harian 4 jam.",
                "Lembur 5,5 jam ini melebihi sisa kuota minggu ini (sisa 1 jam dari 18 jam)."
            ]
        );

        let e = evaluate_quota(&QuotaInput {
            weekly_limit_sec: Some(10 * H),
            weekly_used_sec: 9 * H,
            ..workday()
        });
        assert_eq!(e.view.weekly_limit_minutes, Some(600));
        assert_eq!(
            e.view.messages,
            ["Lembur 2 jam ini melebihi sisa kuota minggu ini (sisa 1 jam dari 10 jam)."]
        );

        let e = evaluate_quota(&QuotaInput {
            requested_sec: 119,
            weekly_used_sec: 59,
            ..workday()
        });
        assert_eq!(e.view.requested_minutes, 1);
        assert_eq!(e.view.weekly_used_minutes, Some(0));
        assert_eq!(e.view.weekly_remaining_minutes, Some(1_079));
    }

    #[test]
    fn quota_holiday_skips_weekly() {
        let e = evaluate_quota(&QuotaInput {
            day_type: DayType::Holiday,
            normal_sec: 0,
            daily_limit_sec: 12 * H,
            requested_sec: 10 * H,
            weekly_used_sec: 18 * H,
            ..workday()
        });
        assert_eq!(e.view.weekly_limit_minutes, None);
        assert_eq!(e.view.weekly_used_minutes, None);
        assert_eq!(e.view.weekly_pending_minutes, None);
        assert_eq!(e.view.weekly_remaining_minutes, None);
        assert_eq!(e.sec.weekly_remaining_sec, None);
        assert!(!e.view.exceeds_weekly && !e.view.exceeds_daily);

        let e = evaluate_quota(&QuotaInput {
            day_type: DayType::Holiday,
            pattern_work_days: 6,
            normal_sec: 0,
            daily_limit_sec: 11 * H,
            requested_sec: 12 * H,
            ..workday()
        });
        assert_eq!(
            e.view.messages,
            ["Lembur 12 jam melebihi batas harian 11 jam."]
        );
    }

    fn row(id: i32, date: &str, sec: Option<i64>, day_type: DayType) -> WeekRow {
        WeekRow {
            id,
            date: d(date),
            day_type: Some(day_type),
            overtime_sec: sec,
        }
    }

    fn flags(rows: &[WeekRow]) -> Vec<(i32, bool)> {
        relabel_week(rows, WEEKLY_LIMIT_SEC)
            .into_iter()
            .map(|f| (f.id, f.exceeds_weekly_limit))
            .collect()
    }

    #[test]
    fn relabel_week_cases() {
        let w = DayType::Workday;
        let hd = DayType::Holiday;
        let rows = [
            row(1, "2026-10-19", Some(4 * H), w),
            row(2, "2026-10-20", Some(4 * H), w),
            row(3, "2026-10-21", Some(4 * H), w),
            row(4, "2026-10-22", Some(4 * H), w),
            row(5, "2026-10-23", Some(3 * H), w),
            row(6, "2026-10-24", Some(0), w),
        ];
        assert_eq!(
            flags(&rows),
            [
                (1, false),
                (2, false),
                (3, false),
                (4, false),
                (5, true),
                (6, true)
            ]
        );

        let exact = [
            row(1, "2026-10-19", Some(4 * H), w),
            row(2, "2026-10-20", Some(4 * H), w),
            row(3, "2026-10-21", Some(4 * H), w),
            row(4, "2026-10-22", Some(4 * H), w),
            row(5, "2026-10-23", Some(2 * H), w),
        ];
        assert!(flags(&exact).iter().all(|(_, f)| !f));

        let unordered = [
            row(9, "2026-10-21", Some(10 * H), w),
            row(3, "2026-10-21", Some(4 * H), w),
            row(7, "2026-10-20", Some(5 * H), w),
        ];
        assert_eq!(flags(&unordered), [(7, false), (3, false), (9, true)]);
        assert_eq!(unordered.first().map(|r| r.id), Some(9));

        let holiday = [
            row(1, "2026-10-18", Some(12 * H), hd),
            row(2, "2026-10-19", Some(10 * H), w),
            row(3, "2026-10-20", Some(8 * H), w),
            row(4, "2026-10-24", Some(11 * H), hd),
            row(5, "2026-10-25", Some(30 * M), w),
        ];
        assert_eq!(
            flags(&holiday),
            [(1, false), (2, false), (3, false), (4, false), (5, true)]
        );

        let nulls = [
            row(1, "2026-10-19", None, w),
            row(2, "2026-10-20", Some(18 * H), w),
        ];
        assert_eq!(flags(&nulls), [(1, false), (2, false)]);
        assert!(flags(&[]).is_empty());

        // dayType kosong diperlakukan bukan hari kerja.
        let missing = [WeekRow {
            id: 1,
            date: d("2026-10-19"),
            day_type: None,
            overtime_sec: Some(20 * H),
        }];
        assert_eq!(flags(&missing), [(1, false)]);
    }

    #[test]
    fn week_start_cases() {
        let cases = [
            ("2026-10-21", 1, "2026-10-19"),
            ("2026-10-19", 1, "2026-10-19"),
            ("2026-10-25", 1, "2026-10-19"),
            ("2026-10-21", 0, "2026-10-18"),
            ("2026-10-18", 0, "2026-10-18"),
            ("2026-10-24", 0, "2026-10-18"),
            ("2026-11-01", 6, "2026-10-31"),
            ("2027-01-01", 1, "2026-12-28"),
        ];
        for (key, start, expected) in cases {
            assert_eq!(week_start_of_key(key, start).as_deref(), Ok(expected));
        }
        assert_eq!(
            week_range_of(d("2026-10-21"), 1),
            Ok((d("2026-10-19"), d("2026-10-25")))
        );
        assert_eq!(
            week_range_of(d("2026-10-21"), 0),
            Ok((d("2026-10-18"), d("2026-10-24")))
        );
        assert!(week_start_of_key("2026-02-30", 1).is_err());
        assert!(week_start_of_key("2026-10-21", 7).is_err());
        assert!(week_start_of_key("2026-10-21", -1).is_err());
    }

    fn iso(ms: i64) -> String {
        mz_core::time::JsDate::from_millis(ms)
            .map(|d| d.to_js_string())
            .unwrap_or_default()
    }

    #[test]
    fn end_of_local_day_cases() {
        let e = |k: &str, p: i64, tz: Option<&str>| {
            end_of_local_day_instant(k, p, tz)
                .map(iso)
                .unwrap_or_default()
        };
        assert_eq!(
            e("2026-10-22", 7, Some("Asia/Jakarta")),
            "2026-10-29T16:59:59.999Z"
        );
        assert_eq!(
            e("2026-10-22", 0, Some("Asia/Jakarta")),
            "2026-10-22T16:59:59.999Z"
        );
        assert_eq!(
            e("2026-10-22", 1, Some("America/New_York")),
            "2026-10-24T03:59:59.999Z"
        );
        assert_eq!(
            e("2026-10-31", 1, Some("America/New_York")),
            "2026-11-02T04:59:59.999Z"
        );
        for tz in [Some("Not/AZone"), None, Some("")] {
            assert_eq!(e("2026-10-22", 2, tz), "2026-10-24T23:59:59.999Z");
        }
        assert!(end_of_local_day_instant("2026-13-01", 0, Some("UTC")).is_err());
    }

    #[test]
    fn next_week_start_cases() {
        let cases = [
            ("2026-10-21", 1, "2026-10-26"),
            ("2026-10-21", 0, "2026-10-25"),
            ("2026-10-19", 1, "2026-10-26"),
            ("2026-10-18", 0, "2026-10-25"),
            ("2026-10-25", 1, "2026-10-26"),
            ("2026-12-30", 1, "2027-01-04"),
        ];
        for (today, start, expected) in cases {
            assert_eq!(next_week_start(d(today), start), Ok(d(expected)));
        }
    }

    #[test]
    fn exceeds_day_cases() {
        assert!(!exceeds_day(24 * H, 0));
        assert!(!exceeds_day(16 * H, 8 * H));
        assert!(exceeds_day(16 * H + 60, 8 * H));
        assert_eq!(
            duration_exceeds_day_message(17 * H, 8 * H),
            "Lembur 17 jam + jam normal 8 jam melebihi 24 jam."
        );
        assert_eq!(
            duration_exceeds_day_message(16 * H + 30 * M, 8 * H),
            "Lembur 16,5 jam + jam normal 8 jam melebihi 24 jam."
        );
    }
}
