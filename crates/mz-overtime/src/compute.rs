//! Perhitungan lembur satu request — port `src/overtime/overtime-compute.service.ts`.
//! Berjalan di transaksi MILIK PEMANGGIL (tick / recalc).

use chrono::{NaiveDate, NaiveDateTime};
use sqlx::PgConnection;

use crate::calendar::{
    DayType, WEEKLY_LIMIT_SEC, day_info_for, load_batch, normal_sec_after_leave,
};
use crate::locks::{lock_week, try_lock_week};
use crate::math::{WeekRow, overtime_sec_for, relabel_week, week_range_of, week_start_of};
use crate::worked::worked_sec;
use crate::zone::{local_date, ms_of, zone_or_utc};

/// `OvertimeComputeOutcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Figur ditulis (APPROVED → COMPLETED atau hitung ulang COMPLETED).
    Computed,
    /// COMPLETED yang hanya diberi `finalizedAt`.
    Finalized,
    /// Tidak ada yang ditulis.
    Skipped,
}

/// T113: berapa kali kunci minggu baru boleh diambil bila `workweekStartDay` berganti.
const MAX_WEEK_START_RECHECKS: usize = 2;

#[derive(sqlx::FromRow)]
struct Row {
    id: i32,
    company_id: i32,
    user_id: i32,
    status: String,
    requested_sec: i32,
    base_normal_sec: Option<i32>,
    day_type: Option<String>,
    final_at: NaiveDateTime,
    finalized_at: Option<NaiveDateTime>,
}

/// `computeOne(tx, requestId, now)`.
pub async fn compute_one(
    conn: &mut PgConnection,
    request_id: i32,
    now: NaiveDateTime,
) -> anyhow::Result<Outcome> {
    // companyId/userId/date tak pernah berubah; status dibaca ulang setelah lock.
    let head: Option<(i32, i32, NaiveDate)> = sqlx::query_as(
        r#"SELECT "companyId", "userId", date FROM "overtimeRequest" WHERE id = $1"#,
    )
    .bind(request_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((company_id, user_id, date)) = head else {
        return Ok(Outcome::Skipped);
    };
    let company: Option<(Option<String>, i32)> =
        sqlx::query_as(r#"SELECT "timeZone", "workweekStartDay" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((time_zone, guessed_week_start)) = company else {
        return Ok(Outcome::Skipped);
    };
    let tz = zone_or_utc(time_zone.as_deref());

    let Some(workweek_start_day) =
        lock_stable_week(conn, company_id, user_id, date, guessed_week_start).await?
    else {
        return Ok(Outcome::Skipped);
    };

    let row: Option<Row> = sqlx::query_as(
        r#"SELECT id, "companyId" AS company_id, "userId" AS user_id, status::text AS status,
                  "requestedSec" AS requested_sec, "baseNormalSec" AS base_normal_sec,
                  "dayType"::text AS day_type, "finalAt" AS final_at, "finalizedAt" AS finalized_at
           FROM "overtimeRequest" WHERE id = $1"#,
    )
    .bind(request_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Ok(Outcome::Skipped);
    };

    let is_approved = row.status == "APPROVED";
    let is_open_completed = row.status == "COMPLETED" && row.finalized_at.is_none();
    if !is_approved && !is_open_completed {
        return Ok(Outcome::Skipped);
    }
    let now_ms = ms_of(now);
    // APPROVED baru dihitung setelah tanggalnya berakhir di zona company (FR-027).
    if is_approved && date >= local_date(now_ms, tz) {
        return Ok(Outcome::Skipped);
    }

    let past_final = ms_of(row.final_at) <= now_ms;
    if is_open_completed && past_final {
        // FR-028: figur dibekukan apa adanya. T8: CAS finalizedAt IS NULL.
        let done = sqlx::query(
            r#"UPDATE "overtimeRequest" SET "finalizedAt" = $2, "updatedAt" = $3
               WHERE id = $1 AND status = 'COMPLETED' AND "finalizedAt" IS NULL"#,
        )
        .bind(row.id)
        .bind(now)
        .bind(mz_core::time::now_naive())
        .execute(&mut *conn)
        .await?;
        return Ok(if done.rows_affected() == 0 {
            Outcome::Skipped
        } else {
            Outcome::Finalized
        });
    }

    let (Some(base_normal_sec), Some(_)) = (row.base_normal_sec, row.day_type.as_deref()) else {
        // APPROVED selalu membawa snapshot (R5); tanpa snapshot jangan menebak jam normal.
        anyhow::bail!(
            "overtimeRequest {} {} tanpa snapshot persetujuan",
            row.id,
            row.status
        );
    };

    let worked_by_user = worked_sec(conn, row.company_id, tz, &[row.user_id], date, date).await?;
    let batch = load_batch(conn, row.company_id, date, date, &[row.user_id]).await?;
    let worked = worked_by_user
        .get(&row.user_id)
        .and_then(|m| m.get(&date))
        .copied()
        .unwrap_or(0);
    // Snapshot (R5) kecuali cuti: cuti dibaca LIVE saat hitung.
    let info = day_info_for(&batch, row.user_id, date, now_ms)?;
    let Some(normal) = normal_sec_after_leave(i64::from(base_normal_sec), info.leave) else {
        anyhow::bail!(
            "overtimeRequest {}: normalSec pecahan ({base_normal_sec} / 2)",
            row.id
        );
    };
    let overtime = overtime_sec_for(worked, normal, i64::from(row.requested_sec));
    let to_i32 = |v: i64, name: &str| {
        i32::try_from(v).map_err(|_| anyhow::anyhow!("{name} di luar Int: {v}"))
    };
    let (worked32, normal32, overtime32) = (
        to_i32(worked, "workedSec")?,
        to_i32(normal, "normalSec")?,
        to_i32(overtime, "overtimeSec")?,
    );

    // T8: CAS. APPROVED → COMPLETED, atau hitung ulang COMPLETED yang belum final.
    // APPROVED lewat finalAt dihitung sekali lalu langsung final.
    let finalize = is_approved && past_final;
    let cas = if is_approved {
        r#"status = 'APPROVED'"#
    } else {
        r#"status = 'COMPLETED' AND "finalizedAt" IS NULL"#
    };
    let finalized_set = if finalize {
        r#", "finalizedAt" = $6"#
    } else {
        ""
    };
    let done = sqlx::query(&format!(
        r#"UPDATE "overtimeRequest"
           SET status = 'COMPLETED', "workedSec" = $2, "normalSec" = $3, "overtimeSec" = $4,
               "computedAt" = $6, "updatedAt" = $5{finalized_set}
           WHERE id = $1 AND {cas}"#
    ))
    .bind(row.id)
    .bind(worked32)
    .bind(normal32)
    .bind(overtime32)
    .bind(mz_core::time::now_naive())
    .bind(now)
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() == 0 {
        return Ok(Outcome::Skipped);
    }

    relabel(conn, row.company_id, row.user_id, date, workweek_start_day).await?;
    Ok(Outcome::Computed)
}

/// T8/T113 `lockStableWeek`: ambil kunci minggu tebakan awal, lalu baca ULANG
/// `workweekStartDay` di bawah kunci. Bila berubah, kunci minggu baru diambil dengan
/// `try` (tanpa hold-and-wait). `None` = skipped (tick berikutnya menyusul).
async fn lock_stable_week(
    conn: &mut PgConnection,
    company_id: i32,
    user_id: i32,
    date: NaiveDate,
    guessed_week_start_day: i32,
) -> anyhow::Result<Option<i32>> {
    let first = week_start_of(date, guessed_week_start_day)?;
    lock_week(conn, company_id, user_id, first).await?;
    let mut held = vec![first];

    for attempt in 0..=MAX_WEEK_START_RECHECKS {
        let fresh: Option<i32> =
            sqlx::query_scalar(r#"SELECT "workweekStartDay" FROM company WHERE id = $1"#)
                .bind(company_id)
                .fetch_optional(&mut *conn)
                .await?;
        let Some(fresh) = fresh else {
            return Ok(None);
        };
        let key = week_start_of(date, fresh)?;
        if held.contains(&key) {
            return Ok(Some(fresh));
        }
        if attempt == MAX_WEEK_START_RECHECKS {
            break;
        }
        if !try_lock_week(conn, company_id, user_id, key).await? {
            return Ok(None);
        }
        held.push(key);
    }
    Ok(None)
}

#[derive(sqlx::FromRow)]
struct WeekDbRow {
    id: i32,
    date: NaiveDate,
    day_type: Option<String>,
    overtime_sec: Option<i32>,
    exceeds_daily_limit: bool,
    exceeds_weekly_limit: bool,
    exceeds_limit: bool,
}

/// R7/FR-032 `relabel`: label mingguan seluruh minggu (rentang dari `workweekStartDay`
/// saat ini), masih di bawah lock T8. Hanya flag yang berubah ditulis, dikelompokkan per
/// nilai target (urutan kelompok = urutan kemunculan pertama, seperti `Map` Node).
async fn relabel(
    conn: &mut PgConnection,
    company_id: i32,
    user_id: i32,
    date: NaiveDate,
    workweek_start_day: i32,
) -> anyhow::Result<()> {
    let (start, end) = week_range_of(date, workweek_start_day)?;
    let rows: Vec<WeekDbRow> = sqlx::query_as(
        r#"SELECT id, date, "dayType"::text AS day_type, "overtimeSec" AS overtime_sec,
                  "exceedsDailyLimit" AS exceeds_daily_limit,
                  "exceedsWeeklyLimit" AS exceeds_weekly_limit, "exceedsLimit" AS exceeds_limit
           FROM "overtimeRequest"
           WHERE "companyId" = $1 AND "userId" = $2 AND status = 'COMPLETED'
             AND date >= $3 AND date <= $4"#,
    )
    .bind(company_id)
    .bind(user_id)
    .bind(start)
    .bind(end)
    .fetch_all(&mut *conn)
    .await?;
    let flags = relabel_week(
        &rows
            .iter()
            .map(|r| WeekRow {
                id: r.id,
                date: r.date,
                day_type: r.day_type.as_deref().and_then(DayType::parse),
                overtime_sec: r.overtime_sec.map(i64::from),
            })
            .collect::<Vec<_>>(),
        WEEKLY_LIMIT_SEC,
    );

    let mut groups: Vec<(bool, bool, Vec<i32>)> = Vec::new();
    for f in flags {
        let Some(r) = rows.iter().find(|r| r.id == f.id) else {
            continue;
        };
        let limit = r.exceeds_daily_limit || f.exceeds_weekly_limit;
        if r.exceeds_weekly_limit == f.exceeds_weekly_limit && r.exceeds_limit == limit {
            continue;
        }
        match groups
            .iter_mut()
            .find(|(w, l, _)| *w == f.exceeds_weekly_limit && *l == limit)
        {
            Some((_, _, ids)) => ids.push(f.id),
            None => groups.push((f.exceeds_weekly_limit, limit, vec![f.id])),
        }
    }
    for (weekly, limit, ids) in groups {
        sqlx::query(
            r#"UPDATE "overtimeRequest" SET "exceedsWeeklyLimit" = $2, "exceedsLimit" = $3, "updatedAt" = $4
               WHERE id = ANY($1) AND status = 'COMPLETED'"#,
        )
        .bind(&ids)
        .bind(weekly)
        .bind(limit)
        .bind(mz_core::time::now_naive())
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}
