//! Port `src/overtime/overtime-quota.service.ts` (specs/004 T044, research R7). Harian =
//! requested vs batas harian; mingguan = Σ `overtimeSec` COMPLETED WORKDAY minggu itu
//! (minggu mulai di `company.workweekStartDay` saat ini). Melebihi hanya meminta
//! konfirmasi (FR-031). Semua query berurutan di koneksi pemanggil (boleh `&mut *tx`,
//! TOCTOU T3).

use std::collections::{BTreeSet, HashMap};
use std::hash::Hash;

use axum::http::StatusCode;
use chrono::NaiveDate;
use mz_core::{ApiError, ApiResult};
use sqlx::PgConnection;

use super::enums::OvertimeDayType;
use super::errors::overtime_error;
use super::mapper::normal_sec_live;
use crate::calendar::{CalendarBatch, DayInfo, day_info_for, load_batch};
use crate::math::{QuotaEvaluationWithSec, QuotaInput, evaluate_quota, week_range_of};

/// `QuotaEvaluateInput`.
#[derive(Debug, Clone)]
pub struct QuotaEvaluateInput {
    pub company_id: i32,
    pub user_id: i32,
    pub date: NaiveDate,
    pub requested_sec: i64,
    /// Versi pola kerja yang dipakai (research R9), epoch ms.
    pub as_of_ms: i64,
    /// Dari [`super::clock::company_clock`] (0 = Minggu … 6 = Sabtu).
    pub workweek_start_day: i32,
    /// Request yang sedang dievaluasi (bila sudah ada) — tak dihitung ke `weeklyPendingMinutes`.
    pub exclude_request_id: Option<i32>,
}

/// `QuotaEvaluateManyItem<K>`.
#[derive(Debug, Clone)]
pub struct QuotaEvaluateManyItem<K> {
    pub key: K,
    pub user_id: i32,
    pub date: NaiveDate,
    pub requested_sec: i64,
    pub exclude_request_id: Option<i32>,
}

/// Baris request aktif (`PENDING_APPROVAL`, `PENDING_EMPLOYEE`, `APPROVED`) untuk
/// `weeklyPendingMinutes` (FX-5.3, informasi saja).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PendingRow {
    pub id: i32,
    pub user_id: i32,
    pub date: NaiveDate,
    pub requested_sec: i32,
    /// Snapshot `dayType`; `None` = belum disetujui (pakai hari live).
    pub day_type: Option<String>,
}

/// Baris COMPLETED WORKDAY untuk pemakaian mingguan.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct UsedRow {
    pub user_id: i32,
    pub date: NaiveDate,
    pub overtime_sec: Option<i32>,
}

fn range_err(e: crate::math::RangeError) -> ApiError {
    ApiError::internal(anyhow::anyhow!("{e}"))
}

/// Murni (`sumPendingSec`): Σ `requestedSec` baris aktif `user_id` di `week` (inklusif) yang
/// hari kerja — snapshot `dayType`, atau hari live `day_type_of(user, date)` bila belum ada.
pub fn sum_pending_sec(
    rows: &[PendingRow],
    user_id: i32,
    week: (NaiveDate, NaiveDate),
    exclude_request_id: Option<i32>,
    mut day_type_of: impl FnMut(i32, NaiveDate) -> anyhow::Result<OvertimeDayType>,
) -> anyhow::Result<i64> {
    let mut sum = 0;
    for r in rows {
        if r.user_id != user_id || Some(r.id) == exclude_request_id {
            continue;
        }
        if r.date < week.0 || r.date > week.1 {
            continue;
        }
        let day_type = match r.day_type.as_deref().and_then(OvertimeDayType::parse) {
            Some(dt) => dt,
            None => day_type_of(r.user_id, r.date)?,
        };
        if day_type == OvertimeDayType::Workday {
            sum += i64::from(r.requested_sec);
        }
    }
    Ok(sum)
}

/// Murni: Σ `overtimeSec ?? 0` baris `user_id` di `week` (inklusif).
pub fn sum_used_sec(rows: &[UsedRow], user_id: i32, week: (NaiveDate, NaiveDate)) -> i64 {
    rows.iter()
        .filter(|r| r.user_id == user_id && r.date >= week.0 && r.date <= week.1)
        .map(|r| i64::from(r.overtime_sec.unwrap_or(0)))
        .sum()
}

/// Murni: `evaluateQuota` dari `DayInfo` tanggal itu + pemakaian/pending mingguan.
pub fn evaluate_from_day(
    date: NaiveDate,
    info: &DayInfo,
    requested_sec: i64,
    weekly_used_sec: i64,
    weekly_pending_sec: i64,
) -> QuotaEvaluationWithSec {
    evaluate_quota(&QuotaInput {
        date: crate::zone::date_key(date),
        day_type: info.day_type,
        pattern_work_days: info.pattern_work_days,
        normal_sec: normal_sec_live(info),
        daily_limit_sec: info.daily_limit_sec,
        requested_sec,
        weekly_used_sec,
        weekly_pending_sec: Some(weekly_pending_sec),
        weekly_limit_sec: None,
    })
}

const PENDING_SQL: &str = r#"SELECT id, "userId" AS user_id, date, "requestedSec" AS requested_sec,
       "dayType"::text AS day_type
  FROM "overtimeRequest"
 WHERE "companyId" = $1 AND "userId" = ANY($2)
   AND status IN ('PENDING_APPROVAL', 'PENDING_EMPLOYEE', 'APPROVED')
   AND EXISTS (SELECT 1 FROM UNNEST($3::date[], $4::date[]) AS w(s, e) WHERE date >= w.s AND date <= w.e)"#;

const USED_SQL: &str = r#"SELECT "userId" AS user_id, date, "overtimeSec" AS overtime_sec
  FROM "overtimeRequest"
 WHERE "companyId" = $1 AND "userId" = ANY($2)
   AND status = 'COMPLETED' AND "dayType" = 'WORKDAY'
   AND EXISTS (SELECT 1 FROM UNNEST($3::date[], $4::date[]) AS w(s, e) WHERE date >= w.s AND date <= w.e)"#;

async fn find_rows<T>(
    conn: &mut PgConnection,
    sql: &'static str,
    company_id: i32,
    user_ids: &[i32],
    weeks: &[(NaiveDate, NaiveDate)],
) -> sqlx::Result<Vec<T>>
where
    T: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> + Send + Unpin,
{
    let starts: Vec<NaiveDate> = weeks.iter().map(|w| w.0).collect();
    let ends: Vec<NaiveDate> = weeks.iter().map(|w| w.1).collect();
    sqlx::query_as::<_, T>(sql)
        .bind(company_id)
        .bind(user_ids)
        .bind(starts)
        .bind(ends)
        .fetch_all(conn)
        .await
}

fn day_type_in(
    batch: &CalendarBatch,
    as_of_ms: i64,
) -> impl FnMut(i32, NaiveDate) -> anyhow::Result<OvertimeDayType> + '_ {
    move |user, date| Ok(day_info_for(batch, user, date, as_of_ms)?.day_type)
}

/// `evaluate(client, input)`: satu tanggal satu karyawan. Dipanggil untuk pratinjau form
/// dan, dengan `conn` = transaksi, di dalam submit/accept/approve (TOCTOU T3).
pub async fn evaluate(
    conn: &mut PgConnection,
    input: &QuotaEvaluateInput,
) -> ApiResult<QuotaEvaluationWithSec> {
    let week = week_range_of(input.date, input.workweek_start_day).map_err(range_err)?;
    // Kalender dimuat se-minggu: baris pending belum punya snapshot `dayType` (hari live).
    let batch = load_batch(conn, input.company_id, week.0, week.1, &[input.user_id]).await?;
    let pending: Vec<PendingRow> = find_rows(
        conn,
        PENDING_SQL,
        input.company_id,
        &[input.user_id],
        &[week],
    )
    .await?;
    let used: Vec<UsedRow> =
        find_rows(conn, USED_SQL, input.company_id, &[input.user_id], &[week]).await?;

    let info = day_info_for(&batch, input.user_id, input.date, input.as_of_ms)?;
    let pending_sec = sum_pending_sec(
        &pending,
        input.user_id,
        week,
        input.exclude_request_id,
        day_type_in(&batch, input.as_of_ms),
    )?;
    Ok(evaluate_from_day(
        input.date,
        &info,
        input.requested_sec,
        sum_used_sec(&used, input.user_id, week),
        pending_sec,
    ))
}

/// `evaluateMany(client, input)`: banyak baris sekaligus (daftar approval, kuota dihitung
/// SEKARANG) — satu batch kalender untuk gabungan minggu, satu query pemakaian, satu query
/// pending. Tanpa item → tanpa query.
pub async fn evaluate_many<K: Clone + Eq + Hash>(
    conn: &mut PgConnection,
    company_id: i32,
    workweek_start_day: i32,
    as_of_ms: i64,
    items: &[QuotaEvaluateManyItem<K>],
) -> ApiResult<HashMap<K, QuotaEvaluationWithSec>> {
    let mut result = HashMap::new();
    if items.is_empty() {
        return Ok(result);
    }
    let mut user_ids: Vec<i32> = Vec::new();
    let mut weeks: BTreeSet<(NaiveDate, NaiveDate)> = BTreeSet::new();
    let mut prepared = Vec::with_capacity(items.len());
    for item in items {
        if !user_ids.contains(&item.user_id) {
            user_ids.push(item.user_id);
        }
        let week = week_range_of(item.date, workweek_start_day).map_err(range_err)?;
        weeks.insert(week);
        prepared.push((item, week));
    }
    let weeks: Vec<(NaiveDate, NaiveDate)> = weeks.into_iter().collect();
    let (Some(from), Some(to)) = (
        weeks.iter().map(|w| w.0).min(),
        weeks.iter().map(|w| w.1).max(),
    ) else {
        return Ok(result);
    };

    let batch = load_batch(conn, company_id, from, to, &user_ids).await?;
    let pending: Vec<PendingRow> =
        find_rows(conn, PENDING_SQL, company_id, &user_ids, &weeks).await?;
    let used: Vec<UsedRow> = find_rows(conn, USED_SQL, company_id, &user_ids, &weeks).await?;

    for (item, week) in prepared {
        let info = day_info_for(&batch, item.user_id, item.date, as_of_ms)?;
        let pending_sec = sum_pending_sec(
            &pending,
            item.user_id,
            week,
            item.exclude_request_id,
            day_type_in(&batch, as_of_ms),
        )?;
        result.insert(
            item.key.clone(),
            evaluate_from_day(
                item.date,
                &info,
                item.requested_sec,
                sum_used_sec(&used, item.user_id, week),
                pending_sec,
            ),
        );
    }
    Ok(result)
}

/// `assertConfirmed(evaluation, confirmOverQuota?)` (TOCTOU T3): melebihi batas tanpa
/// `confirmOverQuota: true` → `409 OVERTIME_QUOTA_CONFIRMATION_REQUIRED` dengan `details`
/// = evaluasi publik (tanpa detik internal). Tidak pernah memblokir bila dikonfirmasi.
pub fn assert_confirmed(
    evaluation: &QuotaEvaluationWithSec,
    confirm_over_quota: Option<bool>,
) -> ApiResult<()> {
    if !(evaluation.view.exceeds_daily || evaluation.view.exceeds_weekly) {
        return Ok(());
    }
    if confirm_over_quota == Some(true) {
        return Ok(());
    }
    Err(overtime_error(
        StatusCode::CONFLICT,
        "OVERTIME_QUOTA_CONFIRMATION_REQUIRED",
        "Lembur ini melebihi batas kuota. Konfirmasi untuk tetap melanjutkan.",
        Some(serde_json::to_value(&evaluation.view).map_err(ApiError::internal)?),
    ))
}

#[cfg(test)]
mod tests {
    //! Kasus murni `overtime-quota.service.spec.ts` (kalender palsu lewat `DayInfo`).
    #![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::calendar::LeaveCoverage;
    use axum::response::IntoResponse;

    const HOUR: i64 = 3600;

    fn d(s: &str) -> NaiveDate {
        crate::zone::parse_date_key(s).unwrap_or_default()
    }

    const WORKDAY: DayInfo = DayInfo {
        day_type: OvertimeDayType::Workday,
        base_normal_sec: 28_800,
        daily_limit_sec: 14_400,
        pattern_work_days: 5,
        leave: LeaveCoverage::None,
    };
    const HOLIDAY: DayInfo = DayInfo {
        day_type: OvertimeDayType::Holiday,
        base_normal_sec: 0,
        daily_limit_sec: 43_200,
        pattern_work_days: 5,
        leave: LeaveCoverage::None,
    };

    fn week() -> (NaiveDate, NaiveDate) {
        week_range_of(d("2026-10-22"), 1).unwrap_or_default()
    }

    fn p(id: i32, user: i32, date: &str, hours: f64, dt: Option<&str>) -> PendingRow {
        PendingRow {
            id,
            user_id: user,
            date: d(date),
            #[allow(clippy::cast_possible_truncation)]
            requested_sec: (hours * 3600.0) as i32,
            day_type: dt.map(str::to_owned),
        }
    }

    fn workday(_: i32, _: NaiveDate) -> anyhow::Result<OvertimeDayType> {
        Ok(OvertimeDayType::Workday)
    }

    #[test]
    fn us1_fresh_quota() {
        assert_eq!(week(), (d("2026-10-19"), d("2026-10-25")));
        let ev = evaluate_from_day(d("2026-10-22"), &WORKDAY, 2 * HOUR, 0, 0).view;
        assert_eq!(ev.normal_minutes, 480);
        assert_eq!(ev.daily_limit_minutes, 240);
        assert_eq!(ev.requested_minutes, 120);
        assert_eq!(ev.daily_remaining_minutes, 240);
        assert_eq!(ev.weekly_limit_minutes, Some(1080));
        assert_eq!(ev.weekly_used_minutes, Some(0));
        assert_eq!(ev.weekly_remaining_minutes, Some(1080));
        assert!(!ev.exceeds_daily && !ev.exceeds_weekly);
        assert!(ev.messages.is_empty());
    }

    #[test]
    fn us1_weekly_exceeded_message() {
        let ev = evaluate_from_day(d("2026-10-22"), &WORKDAY, 3 * HOUR, 16 * HOUR, 0);
        assert!(!ev.view.exceeds_daily);
        assert!(ev.view.exceeds_weekly);
        assert_eq!(ev.view.weekly_used_minutes, Some(960));
        assert_eq!(ev.view.weekly_remaining_minutes, Some(120));
        assert_eq!(
            ev.view.messages,
            vec![
                "Lembur 3 jam ini melebihi sisa kuota minggu ini (sisa 2 jam dari 18 jam)."
                    .to_owned()
            ]
        );
        assert_eq!(ev.sec.weekly_used_sec, Some(16 * HOUR));
    }

    #[test]
    fn holiday_skips_weekly() {
        let ev = evaluate_from_day(d("2026-10-22"), &HOLIDAY, 5 * HOUR, 18 * HOUR, 0).view;
        assert_eq!(ev.normal_minutes, 0);
        assert_eq!(ev.daily_limit_minutes, 720);
        assert_eq!(ev.weekly_limit_minutes, None);
        assert_eq!(ev.weekly_used_minutes, None);
        assert_eq!(ev.weekly_remaining_minutes, None);
        assert_eq!(ev.weekly_pending_minutes, None);
        assert!(!ev.exceeds_weekly && ev.messages.is_empty());
    }

    #[test]
    fn half_leave_cuts_normal() {
        let half = DayInfo {
            leave: LeaveCoverage::Half,
            ..WORKDAY
        };
        assert_eq!(
            evaluate_from_day(d("2026-10-22"), &half, HOUR, 0, 0)
                .view
                .normal_minutes,
            240
        );
    }

    #[test]
    fn week_window_follows_start_day() {
        for (start_day, s, e) in [
            (1, "2026-10-19", "2026-10-25"),
            (0, "2026-10-18", "2026-10-24"),
            (6, "2026-10-17", "2026-10-23"),
        ] {
            assert_eq!(
                week_range_of(d("2026-10-22"), start_day).ok(),
                Some((d(s), d(e)))
            );
        }
    }

    #[test]
    fn pending_sum_in_week_only() {
        let rows = [
            p(1, 7, "2026-10-20", 3.0, None),
            p(2, 7, "2026-10-21", 2.25, Some("WORKDAY")),
            p(3, 7, "2026-10-27", 4.0, None),
        ];
        let sec = sum_pending_sec(&rows, 7, week(), None, workday).unwrap_or_default();
        let ev = evaluate_from_day(d("2026-10-22"), &WORKDAY, HOUR, 0, sec).view;
        assert_eq!(ev.weekly_pending_minutes, Some(315));
        assert!(!ev.exceeds_weekly);
    }

    #[test]
    fn pending_excludes_current_request() {
        let rows = [
            p(1, 7, "2026-10-20", 3.0, None),
            p(2, 7, "2026-10-22", 1.0, None),
        ];
        assert_eq!(
            sum_pending_sec(&rows, 7, week(), Some(2), workday).unwrap_or_default(),
            3 * HOUR
        );
    }

    #[test]
    fn pending_on_holiday_not_counted() {
        let rows = [
            p(1, 7, "2026-10-21", 3.0, None),
            p(2, 7, "2026-10-20", 2.0, Some("HOLIDAY")),
            p(3, 7, "2026-10-20", 1.0, None),
        ];
        let live = |_: i32, date: NaiveDate| {
            Ok(if date == d("2026-10-21") {
                OvertimeDayType::Holiday
            } else {
                OvertimeDayType::Workday
            })
        };
        assert_eq!(
            sum_pending_sec(&rows, 7, week(), None, live).unwrap_or_default(),
            HOUR
        );
    }

    #[test]
    fn many_split_by_user_and_week() {
        let rows = [
            p(1, 7, "2026-10-20", 2.0, None),
            p(2, 7, "2026-10-28", 1.0, None),
            p(3, 8, "2026-10-21", 3.0, None),
        ];
        let w2 = week_range_of(d("2026-10-28"), 1).unwrap_or_default();
        assert_eq!(
            sum_pending_sec(&rows, 7, week(), None, workday).unwrap_or_default(),
            2 * HOUR
        );
        assert_eq!(
            sum_pending_sec(&rows, 7, w2, Some(2), workday).unwrap_or_default(),
            0
        );
        assert_eq!(
            sum_pending_sec(&rows, 8, week(), None, workday).unwrap_or_default(),
            3 * HOUR
        );

        let used = [
            UsedRow {
                user_id: 7,
                date: d("2026-10-19"),
                overtime_sec: Some(36_000),
            },
            UsedRow {
                user_id: 7,
                date: d("2026-10-21"),
                overtime_sec: Some(21_600),
            },
            UsedRow {
                user_id: 7,
                date: d("2026-10-27"),
                overtime_sec: Some(7_200),
            },
            UsedRow {
                user_id: 8,
                date: d("2026-10-20"),
                overtime_sec: Some(3_600),
            },
            UsedRow {
                user_id: 8,
                date: d("2026-10-21"),
                overtime_sec: None,
            },
        ];
        assert_eq!(sum_used_sec(&used, 7, week()), 16 * HOUR);
        assert_eq!(sum_used_sec(&used, 7, w2), 2 * HOUR);
        assert_eq!(sum_used_sec(&used, 8, week()), HOUR);
        let a = evaluate_from_day(d("2026-10-22"), &WORKDAY, 3 * HOUR, 16 * HOUR, 0).view;
        assert_eq!((a.weekly_used_minutes, a.exceeds_weekly), (Some(960), true));
    }

    #[tokio::test]
    async fn assert_confirmed_rules() {
        let exceeding = evaluate_from_day(d("2026-10-22"), &WORKDAY, 3 * HOUR, 16 * HOUR, 0);
        let err = assert_confirmed(&exceeding, None).err();
        let res = err.map(IntoResponse::into_response);
        let status = res.as_ref().map(|r| r.status().as_u16());
        assert_eq!(status, Some(409));
        let body = match res {
            Some(r) => axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap_or_default(),
            None => Default::default(),
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        assert_eq!(v["code"], "OVERTIME_QUOTA_CONFIRMATION_REQUIRED");
        assert_eq!(v["details"]["exceedsWeekly"], true);
        assert_eq!(v["details"]["weeklyRemainingMinutes"], 120);
        assert!(v["details"].get("sec").is_none());

        assert!(assert_confirmed(&exceeding, Some(true)).is_ok());
        let fine = evaluate_from_day(d("2026-10-22"), &WORKDAY, HOUR, 0, 0);
        assert!(assert_confirmed(&fine, None).is_ok());
        assert!(assert_confirmed(&fine, Some(false)).is_ok());
    }
}
