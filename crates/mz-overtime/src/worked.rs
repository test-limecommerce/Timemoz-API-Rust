//! Detik kerja per user per tanggal lokal — port `worked-time/worked-time.intervals.ts`
//! (murni) + `worked-time/worked-time.service.ts` (query).

use std::collections::{BTreeMap, HashMap};

use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime};
use chrono_tz::Tz;
use sqlx::PgConnection;

use crate::zone::{local_day_start_ms, timeline_instant};

/// `[start, end)` epoch ms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interval {
    pub start: i64,
    pub end: i64,
}

/// Margin (hari) scan rentang timeline: zona UTC±14 + data lama jam dinding Jakarta.
const RANGE_MARGIN_DAYS: i64 = 2;

/// `mergeIntervals`: urutkan, satukan yang tumpang tindih/bersentuhan; panjang nol /
/// terbalik dibuang.
pub fn merge_intervals(intervals: &[Interval]) -> Vec<Interval> {
    let mut valid: Vec<Interval> = intervals
        .iter()
        .copied()
        .filter(|i| i.end > i.start)
        .collect();
    valid.sort_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)));
    let mut merged: Vec<Interval> = Vec::new();
    let mut current: Option<Interval> = None;
    for i in valid {
        match current.as_mut() {
            Some(c) if i.start <= c.end => c.end = c.end.max(i.end),
            _ => {
                if let Some(c) = current {
                    merged.push(c);
                }
                current = Some(i);
            }
        }
    }
    if let Some(c) = current {
        merged.push(c);
    }
    merged
}

/// `localDayWindow`: `[D 00:00, D+1 00:00)` lokal sebagai instant (awal hari lokal
/// berikutnya, bukan +86 400 dtk, supaya hari DST benar).
pub fn local_day_window(date: NaiveDate, tz: Tz) -> Interval {
    let next = date.succ_opt().unwrap_or(date);
    Interval {
        start: local_day_start_ms(date, tz),
        end: local_day_start_ms(next, tz),
    }
}

/// `workedSecByDate`: potong ke jendela tanggal lokal, satukan, jumlahkan ms, lalu
/// `floor(ms / 1000)` sekali. Setiap tanggal selalu ada di hasil.
pub fn worked_sec_by_date(
    intervals: &[Interval],
    dates: &[NaiveDate],
    tz: Tz,
) -> BTreeMap<NaiveDate, i64> {
    let mut result = BTreeMap::new();
    for date in dates {
        let w = local_day_window(*date, tz);
        let clipped: Vec<Interval> = intervals
            .iter()
            .filter_map(|i| {
                let start = i.start.max(w.start);
                let end = i.end.min(w.end);
                (end > start).then_some(Interval { start, end })
            })
            .collect();
        let total_ms: i64 = merge_intervals(&clipped)
            .iter()
            .map(|m| m.end - m.start)
            .sum();
        result.insert(*date, total_ms.div_euclid(1000));
    }
    result
}

#[derive(sqlx::FromRow)]
struct TimelineRow {
    id: i32,
    start_at: NaiveDateTime,
    end_at: NaiveDateTime,
    start_at_utc: Option<NaiveDateTime>,
    end_at_utc: Option<NaiveDateTime>,
    timezone: Option<String>,
    user_id: i32,
}

/// Filter dasar `WorkedTimeService.workedSec` (alias `tt` timeline, `tr` tracking):
/// timeline aktif, bukan manual pending/ditolak, project tertaut ke company, tracking aktif
/// milik salah satu user.
const BASE_WHERE: &str = r#"tt.status = 1
  AND (tt."isOffline" = false OR tt."manualTimeStatus" = 'APPROVED')
  AND EXISTS (SELECT 1 FROM project p WHERE p.id = tt."projectId"
              AND EXISTS (SELECT 1 FROM "projectCompany" pc
                          WHERE pc."projectId" = p.id AND pc."companyId" = $1))
  AND tr.status = 1 AND tr."userId" = ANY($2)"#;

const SELECT: &str = r#"SELECT tt.id, tt."startAt" AS start_at, tt."endAt" AS end_at,
  tt."startAtUtc" AS start_at_utc, tt."endAtUtc" AS end_at_utc, tt.timezone, tr."userId" AS user_id
  FROM "trackingTimeline" tt JOIN tracking tr ON tr.id = tt."trackingId""#;

/// `WorkedTimeService.workedSec` untuk `[from, to]` (inklusif). Dua jalur digabung per
/// `id`: (a) `takenDate` ±1 hari, (b) rentang timeline itu sendiri dengan margin 2 hari.
pub async fn worked_sec(
    conn: &mut PgConnection,
    company_id: i32,
    tz: Tz,
    user_ids: &[i32],
    from: NaiveDate,
    to: NaiveDate,
) -> anyhow::Result<HashMap<i32, BTreeMap<NaiveDate, i64>>> {
    if from > to {
        anyhow::bail!("fromKey {from} setelah toKey {to}");
    }
    let mut result = HashMap::new();
    let mut users: Vec<i32> = Vec::new();
    for id in user_ids {
        if !users.contains(id) {
            users.push(*id);
        }
    }
    if users.is_empty() {
        return Ok(result);
    }
    let mut dates = Vec::new();
    let mut d = from;
    while d <= to {
        dates.push(d);
        let Some(next) = d.succ_opt() else { break };
        d = next;
    }

    let by_taken: Vec<TimelineRow> = sqlx::query_as(&format!(
        r#"{SELECT} WHERE {BASE_WHERE} AND tr."takenDate" >= $3 AND tr."takenDate" <= $4"#
    ))
    .bind(company_id)
    .bind(&users)
    .bind(from - Duration::days(1))
    .bind(to + Duration::days(1))
    .fetch_all(&mut *conn)
    .await?;
    let midnight = |d: NaiveDate| d.and_time(NaiveTime::MIN);
    let by_range: Vec<TimelineRow> = sqlx::query_as(&format!(
        r#"{SELECT} WHERE {BASE_WHERE} AND tt."startAt" < $3 AND tt."endAt" > $4"#
    ))
    .bind(company_id)
    .bind(&users)
    .bind(midnight(to + Duration::days(RANGE_MARGIN_DAYS + 1)))
    .bind(midnight(from - Duration::days(RANGE_MARGIN_DAYS)))
    .fetch_all(&mut *conn)
    .await?;

    let mut rows: BTreeMap<i32, TimelineRow> = BTreeMap::new();
    for r in by_taken.into_iter().chain(by_range) {
        rows.insert(r.id, r);
    }
    let mut by_user: HashMap<i32, Vec<Interval>> = users.iter().map(|u| (*u, Vec::new())).collect();
    for r in rows.values() {
        if let Some(list) = by_user.get_mut(&r.user_id) {
            let tz_name = r.timezone.as_deref();
            list.push(Interval {
                start: timeline_instant(r.start_at, r.start_at_utc, tz_name),
                end: timeline_instant(r.end_at, r.end_at_utc, tz_name),
            });
        }
    }
    for (user_id, intervals) in by_user {
        result.insert(user_id, worked_sec_by_date(&intervals, &dates, tz));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(s: &str) -> i64 {
        mz_core::time::parse_js_date(s).map_or(0, crate::zone::ms_of)
    }

    fn iv(a: &str, b: &str) -> Interval {
        Interval {
            start: ms(a),
            end: ms(b),
        }
    }

    fn d(s: &str) -> NaiveDate {
        crate::zone::parse_date_key(s).unwrap_or_default()
    }

    #[test]
    fn merge() {
        assert_eq!(
            merge_intervals(&[
                iv("2026-10-20T01:00:00Z", "2026-10-20T03:00:00Z"),
                iv("2026-10-20T02:00:00Z", "2026-10-20T05:00:00Z")
            ]),
            [iv("2026-10-20T01:00:00Z", "2026-10-20T05:00:00Z")]
        );
        assert_eq!(
            merge_intervals(&[
                iv("2026-10-20T04:00:00Z", "2026-10-20T06:00:00Z"),
                iv("2026-10-20T01:00:00Z", "2026-10-20T04:00:00Z"),
                iv("2026-10-20T08:00:00Z", "2026-10-20T09:00:00Z"),
            ]),
            [
                iv("2026-10-20T01:00:00Z", "2026-10-20T06:00:00Z"),
                iv("2026-10-20T08:00:00Z", "2026-10-20T09:00:00Z")
            ]
        );
        assert_eq!(
            merge_intervals(&[
                iv("2026-10-20T01:00:00Z", "2026-10-20T01:00:00Z"),
                iv("2026-10-20T05:00:00Z", "2026-10-20T04:00:00Z"),
                iv("2026-10-20T02:00:00Z", "2026-10-20T03:00:00Z"),
            ]),
            [iv("2026-10-20T02:00:00Z", "2026-10-20T03:00:00Z")]
        );
        assert!(merge_intervals(&[]).is_empty());
    }

    #[test]
    fn windows() {
        let w = local_day_window(d("2026-10-20"), chrono_tz::Asia::Jakarta);
        assert_eq!(w, iv("2026-10-19T17:00:00Z", "2026-10-20T17:00:00Z"));
        let m = local_day_window(d("2026-10-20"), chrono_tz::Asia::Makassar);
        assert_eq!(m, iv("2026-10-19T16:00:00Z", "2026-10-20T16:00:00Z"));
        let ny = chrono_tz::America::New_York;
        let fall = local_day_window(d("2026-11-01"), ny);
        assert_eq!(fall.end - fall.start, 25 * 3_600_000);
        let spring = local_day_window(d("2026-03-08"), ny);
        assert_eq!(spring.end - spring.start, 23 * 3_600_000);
    }

    #[test]
    fn by_date() {
        let jkt = chrono_tz::Asia::Jakarta;
        let r = worked_sec_by_date(
            &[
                iv("2026-10-20T02:00:00Z", "2026-10-20T06:00:00Z"),
                iv("2026-10-20T04:00:00Z", "2026-10-20T08:00:00Z"),
                iv("2026-10-20T03:00:00Z", "2026-10-20T05:00:00Z"),
            ],
            &[d("2026-10-20")],
            jkt,
        );
        assert_eq!(r.get(&d("2026-10-20")), Some(&(6 * 3600)));
        let r = worked_sec_by_date(
            &[iv("2026-10-19T16:00:00Z", "2026-10-19T18:00:00Z")],
            &[d("2026-10-19"), d("2026-10-20")],
            jkt,
        );
        assert_eq!(r.get(&d("2026-10-19")), Some(&3600));
        assert_eq!(r.get(&d("2026-10-20")), Some(&3600));
        // Sisa milidetik dijumlahkan dulu sebelum floor.
        let r = worked_sec_by_date(
            &[
                iv("2026-10-20T02:00:00.600Z", "2026-10-20T02:00:01.200Z"),
                iv("2026-10-20T03:00:00.000Z", "2026-10-20T03:00:00.600Z"),
            ],
            &[d("2026-10-20")],
            jkt,
        );
        assert_eq!(r.get(&d("2026-10-20")), Some(&1));
        let empty = worked_sec_by_date(&[], &[d("2026-10-20")], jkt);
        assert_eq!(empty.get(&d("2026-10-20")), Some(&0));
    }
}
