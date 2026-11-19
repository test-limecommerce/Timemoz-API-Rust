//! Kalender lembur yang dibutuhkan perhitungan: `calendar/day-info.ts` (klasifikasi
//! tanggal + cakupan cuti), resolver pola kerja `calendar/work-pattern.service.ts`, dan
//! `OvertimeCalendarService.loadBatch/dayInfoFor` (`calendar/overtime-calendar.service.ts`).

use std::collections::HashMap;

use chrono::{Datelike, NaiveDate, NaiveDateTime};
use serde::Serialize;
use sqlx::PgConnection;

use crate::zone::ms_of;

/// `OvertimeDayType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DayType {
    #[serde(rename = "WORKDAY")]
    Workday,
    #[serde(rename = "HOLIDAY")]
    Holiday,
}

impl DayType {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "WORKDAY" => Some(Self::Workday),
            "HOLIDAY" => Some(Self::Holiday),
            _ => None,
        }
    }
}

/// Jam normal per hari (detik).
pub const NORMAL_SEC_FIVE_DAY: i64 = 28_800;
pub const NORMAL_SEC_SIX_DAY: i64 = 25_200;
pub const NORMAL_SEC_SHORT_DAY: i64 = 18_000;
/// Batas lembur harian hari kerja (FR-029).
pub const WORKDAY_DAILY_LIMIT_SEC: i64 = 14_400;
pub const HOLIDAY_DAILY_LIMIT_SEC_FIVE_DAY: i64 = 43_200;
pub const HOLIDAY_DAILY_LIMIT_SEC_SIX_DAY: i64 = 39_600;
/// Batas lembur mingguan (18 jam).
pub const WEEKLY_LIMIT_SEC: i64 = 64_800;

const FULL_DAY: &str = "FULL_DAY";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayPattern {
    pub work_days: i32,
    /// ISO 1 = Senin … 7 = Minggu.
    pub off_days: Vec<i32>,
    pub short_day: Option<i32>,
}

impl DayPattern {
    /// `DEFAULT_PATTERN`: 5 hari, libur Sabtu-Minggu.
    pub fn default_pattern() -> Self {
        Self {
            work_days: 5,
            off_days: vec![6, 7],
            short_day: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Holiday {
    /// `NATIONAL_HOLIDAY` | `JOINT_LEAVE`.
    pub kind: String,
    pub is_active: bool,
    pub deducts_leave: bool,
}

#[derive(Debug, Clone)]
pub struct Leave {
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub start_day_type: String,
    pub end_day_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaveCoverage {
    None,
    Half,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DayInfo {
    pub day_type: DayType,
    pub base_normal_sec: i64,
    pub daily_limit_sec: i64,
    pub pattern_work_days: i32,
    pub leave: LeaveCoverage,
}

fn is_holiday_row(h: &Holiday) -> bool {
    if !h.is_active {
        return false;
    }
    // Cuti bersama yang memotong cuti tahunan tetap hari kerja; selain itu libur.
    if h.kind == "JOINT_LEAVE" {
        return !h.deducts_leave;
    }
    true
}

pub fn leave_coverage(date: NaiveDate, leaves: &[Leave]) -> LeaveCoverage {
    let mut result = LeaveCoverage::None;
    for l in leaves {
        if date < l.start_date || date > l.end_date {
            continue;
        }
        let half = (date == l.start_date && l.start_day_type != FULL_DAY)
            || (date == l.end_date && l.end_date != l.start_date && l.end_day_type != FULL_DAY);
        if !half {
            return LeaveCoverage::Full; // full menang atas half
        }
        result = LeaveCoverage::Half;
    }
    result
}

/// `dayInfo(input)`. `Err` = `workDays` bukan 5/6 (Node melempar `Error`).
pub fn day_info(
    date: NaiveDate,
    pattern: &DayPattern,
    holidays_on_date: &[Holiday],
    leaves_on_date: &[Leave],
) -> anyhow::Result<DayInfo> {
    if pattern.work_days != 5 && pattern.work_days != 6 {
        anyhow::bail!("workDays harus 5 atau 6, bukan {}", pattern.work_days);
    }
    let weekday = date.weekday().number_from_monday() as i32;
    let pattern_work_days = if pattern.work_days == 6 { 6 } else { 5 };
    let is_day_off = pattern.off_days.contains(&weekday);
    let is_holiday = is_day_off || holidays_on_date.iter().any(is_holiday_row);
    let leave = leave_coverage(date, leaves_on_date);

    if is_holiday {
        return Ok(DayInfo {
            day_type: DayType::Holiday,
            base_normal_sec: 0,
            daily_limit_sec: if pattern_work_days == 6 {
                HOLIDAY_DAILY_LIMIT_SEC_SIX_DAY
            } else {
                HOLIDAY_DAILY_LIMIT_SEC_FIVE_DAY
            },
            pattern_work_days,
            leave,
        });
    }
    let base_normal_sec = if pattern_work_days == 6 {
        if pattern.short_day == Some(weekday) {
            NORMAL_SEC_SHORT_DAY
        } else {
            NORMAL_SEC_SIX_DAY
        }
    } else {
        NORMAL_SEC_FIVE_DAY
    };
    Ok(DayInfo {
        day_type: DayType::Workday,
        base_normal_sec,
        daily_limit_sec: WORKDAY_DAILY_LIMIT_SEC,
        pattern_work_days,
        leave,
    })
}

/// `normalSecAfterLeave` (R1.1): full 0, half = setengah jam normal hari itu.
/// `None` = hasil pecahan (Node menulis pecahan ke kolom `Int` → Prisma menolak).
pub fn normal_sec_after_leave(base_normal_sec: i64, leave: LeaveCoverage) -> Option<i64> {
    match leave {
        LeaveCoverage::Full => Some(0),
        LeaveCoverage::Half => (base_normal_sec % 2 == 0).then_some(base_normal_sec / 2),
        LeaveCoverage::None => Some(base_normal_sec),
    }
}

/// Baris `companyWorkPattern` ternormalisasi.
#[derive(Debug, Clone)]
pub struct WorkPatternRow {
    pub id: i32,
    pub work_days: i32,
    pub off_days: Vec<i32>,
    pub short_day: Option<i32>,
    pub effective_from: NaiveDate,
    pub created_at_ms: i64,
    pub superseded_at_ms: Option<i64>,
}

/// Kandidat pada waktu `as_of_ms`: sudah dibuat dan belum digantikan.
pub fn is_candidate(row: &WorkPatternRow, as_of_ms: i64) -> bool {
    row.created_at_ms <= as_of_ms && row.superseded_at_ms.is_none_or(|s| s > as_of_ms)
}

/// Pemenang: effectiveFrom tertinggi, lalu createdAt terbaru (id pemutus seri).
pub fn wins(a: &WorkPatternRow, b: &WorkPatternRow) -> bool {
    if a.effective_from != b.effective_from {
        return a.effective_from > b.effective_from;
    }
    if a.created_at_ms != b.created_at_ms {
        return a.created_at_ms > b.created_at_ms;
    }
    a.id > b.id
}

/// Pemenang di antara `rows` menurut [`wins`] (`None` bila kosong).
pub fn pick_winner<'a>(
    rows: impl Iterator<Item = &'a WorkPatternRow>,
) -> Option<&'a WorkPatternRow> {
    let mut winner: Option<&WorkPatternRow> = None;
    for row in rows {
        if winner.is_none_or(|w| wins(row, w)) {
            winner = Some(row);
        }
    }
    winner
}

/// Pola dari satu baris (`offDays` terurut).
pub fn pattern_of(row: &WorkPatternRow) -> DayPattern {
    let mut off_days = row.off_days.clone();
    off_days.sort_unstable();
    DayPattern {
        work_days: row.work_days,
        off_days,
        short_day: row.short_day,
    }
}

/// `resolvePatternFromRows` + `effectiveFrom` pemenang (`None` = default kode). Dipakai
/// `mz-companies` (blok `current`); [`resolve_pattern`] hanya mengambil polanya.
pub fn resolve_pattern_with_from(
    rows: &[WorkPatternRow],
    date: NaiveDate,
    as_of_ms: i64,
) -> (DayPattern, Option<NaiveDate>) {
    let winner = pick_winner(
        rows.iter()
            .filter(|r| r.effective_from <= date && is_candidate(r, as_of_ms)),
    );
    match winner {
        Some(w) => (pattern_of(w), Some(w.effective_from)),
        None => (DayPattern::default_pattern(), None),
    }
}

/// `resolvePatternFromRows`: kandidat `effectiveFrom ≤ date`, `createdAt ≤ asOf`,
/// (`supersededAt` null atau `> asOf`). Tak ada kandidat → default.
pub fn resolve_pattern(rows: &[WorkPatternRow], date: NaiveDate, as_of_ms: i64) -> DayPattern {
    resolve_pattern_with_from(rows, date, as_of_ms).0
}

/// Hasil `loadBatch` untuk rentang `[from, to]`.
#[derive(Debug, Clone)]
pub struct CalendarBatch {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub pattern_rows: Vec<WorkPatternRow>,
    pub holidays_by_date: HashMap<NaiveDate, Vec<Holiday>>,
    pub leaves_by_user: HashMap<i32, Vec<Leave>>,
}

#[derive(sqlx::FromRow)]
struct PatternDbRow {
    id: i32,
    work_days: i16,
    off_days: Vec<i32>,
    short_day: Option<i32>,
    effective_from: NaiveDate,
    created_at: NaiveDateTime,
    superseded_at: Option<NaiveDateTime>,
}

/// `OvertimeCalendarService.loadBatch`: tiga query (pola kerja, libur aktif dalam
/// rentang, cuti APPROVED beririsan untuk `user_ids`). Dijalankan berurutan di koneksi
/// transaksi pemanggil.
pub async fn load_batch(
    conn: &mut PgConnection,
    company_id: i32,
    from: NaiveDate,
    to: NaiveDate,
    user_ids: &[i32],
) -> anyhow::Result<CalendarBatch> {
    if from > to {
        anyhow::bail!("Rentang terbalik: {from} > {to}");
    }
    let mut unique: Vec<i32> = Vec::new();
    for id in user_ids {
        if !unique.contains(id) {
            unique.push(*id);
        }
    }

    // `WorkPatternService.loadRows`: semua baris `effectiveFrom ≤ to` (tanpa batas bawah).
    let patterns: Vec<PatternDbRow> = sqlx::query_as(
        r#"SELECT id, "workDays" AS work_days, "offDays" AS off_days, "shortDay" AS short_day,
                  "effectiveFrom" AS effective_from, "createdAt" AS created_at,
                  "supersededAt" AS superseded_at
           FROM "companyWorkPattern"
           WHERE "companyId" = $1 AND "effectiveFrom" <= $2"#,
    )
    .bind(company_id)
    .bind(to)
    .fetch_all(&mut *conn)
    .await?;

    let holidays: Vec<(NaiveDate, String, bool, bool)> = sqlx::query_as(
        r#"SELECT date, type::text, "isActive", "deductsLeave" FROM "companyHoliday"
           WHERE "companyId" = $1 AND "isActive" = true AND date >= $2 AND date <= $3"#,
    )
    .bind(company_id)
    .bind(from)
    .bind(to)
    .fetch_all(&mut *conn)
    .await?;

    let leaves: Vec<(i32, NaiveDate, NaiveDate, String, String)> = sqlx::query_as(
        r#"SELECT "userId", "startDate", "endDate", "startDayType"::text, "endDayType"::text
           FROM "leaveRequest"
           WHERE "companyId" = $1 AND status = 'APPROVED' AND "userId" = ANY($2)
             AND "startDate" <= $3 AND "endDate" >= $4"#,
    )
    .bind(company_id)
    .bind(&unique)
    .bind(to)
    .bind(from)
    .fetch_all(&mut *conn)
    .await?;

    let pattern_rows = patterns
        .into_iter()
        .map(|r| {
            let mut off_days = r.off_days;
            off_days.sort_unstable();
            WorkPatternRow {
                id: r.id,
                work_days: i32::from(r.work_days),
                off_days,
                short_day: r.short_day,
                effective_from: r.effective_from,
                created_at_ms: ms_of(r.created_at),
                superseded_at_ms: r.superseded_at.map(ms_of),
            }
        })
        .collect();

    let mut holidays_by_date: HashMap<NaiveDate, Vec<Holiday>> = HashMap::new();
    for (date, kind, is_active, deducts_leave) in holidays {
        holidays_by_date.entry(date).or_default().push(Holiday {
            kind,
            is_active,
            deducts_leave,
        });
    }
    let mut leaves_by_user: HashMap<i32, Vec<Leave>> =
        unique.iter().map(|id| (*id, Vec::new())).collect();
    for (user_id, start_date, end_date, start_day_type, end_day_type) in leaves {
        if let Some(list) = leaves_by_user.get_mut(&user_id) {
            list.push(Leave {
                start_date,
                end_date,
                start_day_type,
                end_day_type,
            });
        }
    }
    Ok(CalendarBatch {
        from,
        to,
        pattern_rows,
        holidays_by_date,
        leaves_by_user,
    })
}

/// `OvertimeCalendarService.dayInfoFor`.
pub fn day_info_for(
    batch: &CalendarBatch,
    user_id: i32,
    date: NaiveDate,
    as_of_ms: i64,
) -> anyhow::Result<DayInfo> {
    let Some(leaves) = batch.leaves_by_user.get(&user_id) else {
        anyhow::bail!("userId {user_id} tidak ada di batch");
    };
    if date < batch.from || date > batch.to {
        anyhow::bail!("Di luar rentang yang dimuat: {date}");
    }
    let pattern = resolve_pattern(&batch.pattern_rows, date, as_of_ms);
    day_info(
        date,
        &pattern,
        batch
            .holidays_by_date
            .get(&date)
            .map(Vec::as_slice)
            .unwrap_or_default(),
        leaves,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        crate::zone::parse_date_key(s).unwrap_or_default()
    }

    fn leave(a: &str, b: &str, s: &str, e: &str) -> Leave {
        Leave {
            start_date: d(a),
            end_date: d(b),
            start_day_type: s.into(),
            end_day_type: e.into(),
        }
    }

    #[test]
    fn coverage() {
        let multi = leave(
            "2026-10-20",
            "2026-10-22",
            "HALF_DAY_AFTERNOON",
            "HALF_DAY_MORNING",
        );
        assert_eq!(
            leave_coverage(d("2026-10-20"), std::slice::from_ref(&multi)),
            LeaveCoverage::Half
        );
        assert_eq!(
            leave_coverage(d("2026-10-21"), std::slice::from_ref(&multi)),
            LeaveCoverage::Full
        );
        assert_eq!(
            leave_coverage(d("2026-10-22"), std::slice::from_ref(&multi)),
            LeaveCoverage::Half
        );
        assert_eq!(
            leave_coverage(d("2026-10-23"), std::slice::from_ref(&multi)),
            LeaveCoverage::None
        );
        // Satu hari: hanya startDayType yang menentukan.
        let single = leave("2026-10-20", "2026-10-20", "FULL_DAY", "HALF_DAY_MORNING");
        assert_eq!(
            leave_coverage(d("2026-10-20"), &[single]),
            LeaveCoverage::Full
        );
        // full menang atas half.
        let half = leave("2026-10-20", "2026-10-20", "HALF_DAY_MORNING", "FULL_DAY");
        let full = leave("2026-10-19", "2026-10-21", "FULL_DAY", "FULL_DAY");
        assert_eq!(
            leave_coverage(d("2026-10-20"), &[half, full]),
            LeaveCoverage::Full
        );
    }

    #[test]
    fn classification() {
        let p5 = DayPattern::default_pattern();
        // 2026-10-24 = Sabtu.
        let sat = day_info(d("2026-10-24"), &p5, &[], &[]).ok();
        assert_eq!(
            sat.map(|i| (i.day_type, i.base_normal_sec, i.daily_limit_sec)),
            Some((DayType::Holiday, 0, 43_200))
        );
        let wed = day_info(d("2026-10-21"), &p5, &[], &[]).ok();
        assert_eq!(
            wed.map(|i| (i.day_type, i.base_normal_sec)),
            Some((DayType::Workday, 28_800))
        );
        let joint = Holiday {
            kind: "JOINT_LEAVE".into(),
            is_active: true,
            deducts_leave: true,
        };
        let w = day_info(d("2026-10-21"), &p5, &[joint], &[]).ok();
        assert_eq!(w.map(|i| i.day_type), Some(DayType::Workday));
        let national = Holiday {
            kind: "NATIONAL_HOLIDAY".into(),
            is_active: true,
            deducts_leave: false,
        };
        let h = day_info(d("2026-10-21"), &p5, &[national], &[]).ok();
        assert_eq!(h.map(|i| i.day_type), Some(DayType::Holiday));
        let p6 = DayPattern {
            work_days: 6,
            off_days: vec![7],
            short_day: Some(6),
        };
        let short = day_info(d("2026-10-24"), &p6, &[], &[]).ok();
        assert_eq!(
            short.map(|i| (i.base_normal_sec, i.pattern_work_days)),
            Some((18_000, 6))
        );
        let sun6 = day_info(d("2026-10-25"), &p6, &[], &[]).ok();
        assert_eq!(sun6.map(|i| i.daily_limit_sec), Some(39_600));
        let bad = DayPattern {
            work_days: 4,
            off_days: vec![],
            short_day: None,
        };
        assert!(day_info(d("2026-10-21"), &bad, &[], &[]).is_err());
    }

    #[test]
    fn after_leave() {
        assert_eq!(
            normal_sec_after_leave(28_800, LeaveCoverage::Half),
            Some(14_400)
        );
        assert_eq!(normal_sec_after_leave(28_800, LeaveCoverage::Full), Some(0));
        assert_eq!(
            normal_sec_after_leave(25_200, LeaveCoverage::None),
            Some(25_200)
        );
        assert_eq!(normal_sec_after_leave(7, LeaveCoverage::Half), None);
    }

    #[test]
    fn pattern_resolution() {
        let row = |id, from: &str, created, superseded, work_days| WorkPatternRow {
            id,
            work_days,
            off_days: vec![7, 6],
            short_day: None,
            effective_from: d(from),
            created_at_ms: created,
            superseded_at_ms: superseded,
        };
        let rows = vec![
            row(1, "2026-01-05", 0, None, 5),
            row(2, "2026-10-19", 100, None, 6),
            row(3, "2026-10-19", 200, Some(300), 5),
        ];
        assert_eq!(resolve_pattern(&rows, d("2026-10-18"), 1_000).work_days, 5);
        assert_eq!(resolve_pattern(&rows, d("2026-10-20"), 1_000).work_days, 6);
        // asOf 250: baris 3 (createdAt 200) belum digantikan dan menang atas baris 2.
        let at250 = resolve_pattern(&rows, d("2026-10-20"), 250);
        assert_eq!(at250.work_days, 5);
        assert_eq!(at250.off_days, vec![6, 7]);
        assert_eq!(
            resolve_pattern(&[], d("2026-10-20"), 0),
            DayPattern::default_pattern()
        );
    }
}
