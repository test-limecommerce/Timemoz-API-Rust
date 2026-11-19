//! Kalender kerja yang dipakai domain cuti:
//! - `WorkPatternService.offDayPredicate` (`overtime/calendar/work-pattern.service.ts:296`) di
//!   atas resolver pola milik `mz_overtime` (`calendar::resolve_pattern`,
//!   `hr::common::clock::load_pattern_rows`).
//! - `loadHolidayIndex` (`leaves/holiday-calendar.ts`).

use std::collections::{HashMap, HashSet};

use chrono::{Datelike, NaiveDate, NaiveDateTime};
use mz_overtime::calendar::{WorkPatternRow, is_candidate, resolve_pattern};
use mz_overtime::zone::ms_of;
use sqlx::PgConnection;

/// Predikat hari libur mingguan yang barisnya sudah dimuat untuk `[from, to]`.
/// Tanggal setelah `to` tidak boleh ditanyakan (Node melempar `RangeError`); pemanggil yang
/// bisa melewati batas memakai [`OffDays::covers`] lalu memuat ulang (`offDaysCovering`).
#[derive(Debug, Clone)]
pub struct OffDays {
    to: NaiveDate,
    rows: Vec<WorkPatternRow>,
    as_of_ms: i64,
}

impl OffDays {
    /// Tanpa baris pola (`DEFAULT_PATTERN`: Sabtu & Minggu), untuk uji/pemakaian murni.
    pub fn default_pattern(to: NaiveDate) -> Self {
        Self {
            to,
            rows: Vec::new(),
            as_of_ms: 0,
        }
    }

    /// Dari baris yang sudah dimuat (murni).
    pub fn from_rows(rows: Vec<WorkPatternRow>, to: NaiveDate, as_of: NaiveDateTime) -> Self {
        let as_of_ms = ms_of(as_of);
        let rows = rows
            .into_iter()
            .filter(|r| is_candidate(r, as_of_ms))
            .collect();
        Self { to, rows, as_of_ms }
    }

    /// Batas atas rentang muat.
    pub fn to(&self) -> NaiveDate {
        self.to
    }

    /// `dateKey <= toKey`.
    pub fn covers(&self, d: NaiveDate) -> bool {
        d <= self.to
    }

    /// Apakah `d` hari libur mingguan pola yang berlaku pada `d` (ISO 1=Sen..7=Min).
    pub fn is_off(&self, d: NaiveDate) -> bool {
        let weekday = i32::try_from(d.weekday().number_from_monday()).unwrap_or(0);
        resolve_pattern(&self.rows, d, self.as_of_ms)
            .off_days
            .contains(&weekday)
    }
}

/// `offDayPredicate(client, companyId, fromKey, toKey, asOf)`: satu query
/// (`effectiveFrom <= to`), kandidat disaring menurut `as_of`.
pub async fn off_day_predicate(
    conn: &mut PgConnection,
    company_id: i32,
    _from: NaiveDate,
    to: NaiveDate,
    as_of: NaiveDateTime,
) -> sqlx::Result<OffDays> {
    let rows = mz_overtime::hr::common::clock::load_pattern_rows(conn, company_id, to).await?;
    Ok(OffDays::from_rows(rows, to, as_of))
}

/// `HolidayIndexEntry`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HolidayIndexEntry {
    pub id: i32,
    pub name: String,
    pub deducts_leave: bool,
}

/// `HolidayIndex`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HolidayIndex {
    pub excluded_dates: HashSet<NaiveDate>,
    pub by_date: HashMap<NaiveDate, HolidayIndexEntry>,
}

impl HolidayIndex {
    /// Bangun dari baris `(id, name, date, deductsLeave)` terurut tanggal naik. Dua libur di
    /// tanggal sama: yang memotong cuti menang.
    pub fn from_rows(rows: impl IntoIterator<Item = (i32, String, NaiveDate, bool)>) -> Self {
        let mut idx = Self::default();
        for (id, name, date, deducts_leave) in rows {
            idx.excluded_dates.insert(date);
            let replace = idx
                .by_date
                .get(&date)
                .is_none_or(|e| !e.deducts_leave && deducts_leave);
            if replace {
                idx.by_date.insert(
                    date,
                    HolidayIndexEntry {
                        id,
                        name,
                        deducts_leave,
                    },
                );
            }
        }
        idx
    }
}

/// `loadHolidayIndex(db, companyId, fromKey, toKey)`: libur AKTIF company di `[from, to]`.
pub async fn load_holiday_index(
    conn: &mut PgConnection,
    company_id: i32,
    from: NaiveDate,
    to: NaiveDate,
) -> sqlx::Result<HolidayIndex> {
    let rows: Vec<(i32, String, NaiveDate, bool)> = sqlx::query_as(
        r#"SELECT id, name, date, "deductsLeave" FROM "companyHoliday"
           WHERE "companyId" = $1 AND "isActive" = true AND date >= $2 AND date <= $3
           ORDER BY date ASC"#,
    )
    .bind(company_id)
    .bind(from)
    .bind(to)
    .fetch_all(conn)
    .await?;
    Ok(HolidayIndex::from_rows(rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveTime;

    fn d(s: &str) -> NaiveDate {
        crate::day_count::parse_date_key(s).unwrap_or_default()
    }

    fn at(s: &str) -> NaiveDateTime {
        mz_core::time::parse_js_date(s).unwrap_or_default()
    }

    fn six_day(effective_from: &str, created_at: &str) -> WorkPatternRow {
        WorkPatternRow {
            id: 1,
            work_days: 6,
            off_days: vec![7],
            short_day: Some(6),
            effective_from: d(effective_from),
            created_at_ms: ms_of(at(created_at)),
            superseded_at_ms: None,
        }
    }

    #[test]
    fn default_is_sat_sun() {
        let off = OffDays::default_pattern(d("2026-12-31"));
        assert!(off.is_off(d("2026-10-03")) && off.is_off(d("2026-10-04")));
        assert!(!off.is_off(d("2026-10-02")));
        assert!(off.covers(d("2026-12-31")) && !off.covers(d("2027-01-01")));
    }

    // holiday-deduction.service.spec.ts › scheduled pattern: berlaku mulai effectiveFrom saja.
    #[test]
    fn scheduled_pattern_applies_from_effective_date() {
        let now = d("2026-10-12").and_time(NaiveTime::MIN);
        let off = OffDays::from_rows(
            vec![six_day("2026-10-05", "2026-09-30T00:00:00Z")],
            d("2026-10-10"),
            now,
        );
        assert!(
            off.is_off(d("2026-10-03")),
            "Sabtu sebelum effectiveFrom = libur"
        );
        assert!(
            !off.is_off(d("2026-10-10")),
            "Sabtu sesudahnya = hari kerja"
        );
        assert!(off.is_off(d("2026-10-11")), "Minggu tetap libur");
    }

    // FR-046: pola yang dibuat SETELAH asOf tidak berlaku.
    #[test]
    fn as_of_filters_rows_created_later() {
        let rows = vec![six_day("2026-09-28", "2026-09-26T00:00:00Z")];
        let before = OffDays::from_rows(rows.clone(), d("2026-10-03"), at("2026-09-25T03:00:00Z"));
        assert!(before.is_off(d("2026-10-03")));
        let after = OffDays::from_rows(rows, d("2026-10-03"), at("2026-09-27T03:00:00Z"));
        assert!(!after.is_off(d("2026-10-03")));
    }

    #[test]
    fn holiday_index_prefers_deducting() {
        let idx = HolidayIndex::from_rows(vec![
            (1, "A".to_owned(), d("2026-10-01"), false),
            (2, "B".to_owned(), d("2026-10-01"), true),
            (3, "C".to_owned(), d("2026-10-01"), false),
            (4, "D".to_owned(), d("2026-10-02"), false),
        ]);
        assert_eq!(idx.excluded_dates.len(), 2);
        assert_eq!(idx.by_date.get(&d("2026-10-01")).map(|e| e.id), Some(2));
        assert_eq!(idx.by_date.get(&d("2026-10-02")).map(|e| e.id), Some(4));
    }
}
