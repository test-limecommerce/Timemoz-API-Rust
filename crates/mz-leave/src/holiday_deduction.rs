//! `HolidayDeductionService` (`leave-schedule/holiday-deduction.service.ts`): potongan cuti
//! bersama / libur pemotong cuti dan hitung ulang cuti tersetujui. Idempotensi berdasarkan
//! status baris `holidayLeaveDeduction`, bukan jam berjalan. Satu transaksi pendek per
//! karyawan; kunci `leaveBalance` selalu diambil lebih dulu.

use std::collections::HashSet;

use chrono::{NaiveDate, NaiveDateTime};
use mz_core::time::now_naive;
use sqlx::{PgConnection, PgPool};

use crate::balance::{
    BalanceDelta, NewAdjustment, apply_balance_delta, find_balance, get_or_create_balance,
    insert_adjustment, lock_balance,
};
use crate::calendar::{OffDays, load_holiday_index, off_day_predicate};
use crate::day_count::{count_leave_days, is_sat_sun};
use crate::enums::{
    HOLIDAY_DEDUCTION_APPLIED, HOLIDAY_DEDUCTION_REVERSED, LeaveBalanceChangeSource,
    LeaveBalanceSource, LeaveDayType, LeaveSickMode, LeaveStatus, LeaveType,
};
use crate::js::{num_str, round2};
use crate::math::{BalanceAmounts, carry_available, resolve_sick_mode, split_deduction};
use crate::models::{DEDUCTION_COLUMNS, HolidayLeaveDeduction, HolidayState};
use crate::policy::{load_policy, local_day_start, local_now, policy_carry_over_expired};

/// `{ reversed, applied }`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeductionRun {
    pub reversed: u32,
    pub applied: u32,
}

/// `needsReversal(row, holiday, isWeeklyOff)`: apakah potongan APPLIED harus dibatalkan dari
/// kondisi libur saat ini. `is_weekly_off = None` → Sabtu & Minggu.
pub fn needs_reversal(
    row_holiday_id: Option<i32>,
    row_holiday_date: NaiveDate,
    holiday: Option<&HolidayState>,
    is_weekly_off: Option<&dyn Fn(NaiveDate) -> bool>,
) -> bool {
    let Some(h) = holiday.filter(|_| row_holiday_id.is_some()) else {
        return true;
    };
    if !h.is_active || !h.deducts_leave {
        return true;
    }
    if h.date != row_holiday_date {
        return true;
    }
    match is_weekly_off {
        Some(f) => f(h.date),
        None => is_sat_sun(h.date),
    }
}

/// `offDaysCovering`: pakai predikat yang sudah dimuat bila mencakup `date`; selain itu
/// (tanggal libur bergeser melewati rentang muat) muat ulang dengan `as_of` = `now` run.
async fn off_days_covering(
    conn: &mut PgConnection,
    company_id: i32,
    date: Option<NaiveDate>,
    loaded: &OffDays,
    now: NaiveDateTime,
) -> sqlx::Result<Option<OffDays>> {
    match date {
        Some(d) if !loaded.covers(d) => {
            Ok(Some(off_day_predicate(conn, company_id, d, d, now).await?))
        }
        _ => Ok(None),
    }
}

async fn load_holiday(conn: &mut PgConnection, id: i32) -> sqlx::Result<Option<HolidayState>> {
    sqlx::query_as(
        r#"SELECT id, name, date, "isActive", "deductsLeave" FROM "companyHoliday" WHERE id = $1"#,
    )
    .bind(id)
    .fetch_optional(conn)
    .await
}

/// `applyDue(companyId, timeZone, now)` (tick per jam).
pub async fn apply_due(
    pool: &PgPool,
    company_id: i32,
    time_zone: &str,
    now: NaiveDateTime,
) -> sqlx::Result<DeductionRun> {
    run(pool, company_id, None, time_zone, now).await
}

/// `reconcileHoliday(companyId, holidayId, timeZone, now)` (job `leave.holiday-sync`).
pub async fn reconcile_holiday(
    pool: &PgPool,
    company_id: i32,
    holiday_id: Option<i32>,
    time_zone: &str,
    now: NaiveDateTime,
) -> sqlx::Result<DeductionRun> {
    run(pool, company_id, holiday_id, time_zone, now).await
}

async fn run(
    pool: &PgPool,
    company_id: i32,
    holiday_id: Option<i32>,
    time_zone: &str,
    now: NaiveDateTime,
) -> sqlx::Result<DeductionRun> {
    let reversed = reverse_pass(pool, company_id, holiday_id, time_zone, now).await?;
    let applied = apply_pass(pool, company_id, holiday_id, time_zone, now).await?;
    Ok(DeductionRun { reversed, applied })
}

// ── Reverse ──────────────────────────────────────────────────────────────────

/// SQL kandidat `reversePass`. Padanan per syarat `needsReversal`:
/// holidayId null / libur hilang → `h.id IS NULL`; `!isActive`/`!deductsLeave`; tanggal
/// bergeser → `h."date" <> d."holidayDate"`. Libur mingguan pola TIDAK di SQL (diputuskan di
/// dalam transaksi). Cakupan per libur: `d."holidayId" = $3 OR d."holidayId" IS NULL`.
pub const REVERSE_CANDIDATES_SQL: &str = r#"SELECT d.id, d."balanceId"
      FROM "holidayLeaveDeduction" d
      LEFT JOIN "companyHoliday" h ON h.id = d."holidayId"
      WHERE d."companyId" = $1
        AND d.status = $2
        AND ($3::int IS NULL OR d."holidayId" = $3 OR d."holidayId" IS NULL)
        AND (
          h.id IS NULL
          OR NOT h."isActive"
          OR NOT h."deductsLeave"
          OR h."date" <> d."holidayDate"
        )
      ORDER BY d.id"#;

async fn reverse_pass(
    pool: &PgPool,
    company_id: i32,
    holiday_id: Option<i32>,
    time_zone: &str,
    now: NaiveDateTime,
) -> sqlx::Result<u32> {
    let rows: Vec<(i32, i32)> = sqlx::query_as(REVERSE_CANDIDATES_SQL)
        .bind(company_id)
        .bind(HOLIDAY_DEDUCTION_APPLIED)
        .bind(holiday_id)
        .fetch_all(pool)
        .await?;
    if rows.is_empty() {
        return Ok(0);
    }
    // Satu query pola per pass: semua potongan APPLIED bertanggal ≤ hari ini (lokal).
    let today = local_now(now, time_zone).date();
    let off = {
        let mut conn = pool.acquire().await?;
        off_day_predicate(&mut conn, company_id, today, today, now).await?
    };

    let mut reversed = 0;
    for (id, balance_id) in rows {
        match reverse_one(pool, company_id, id, balance_id, &off, now).await {
            Ok(true) => reversed += 1,
            Ok(false) => {}
            Err(e) => tracing::error!("Pembatalan potongan libur {id} gagal: {e}"),
        }
    }
    Ok(reversed)
}

async fn reverse_one(
    pool: &PgPool,
    company_id: i32,
    id: i32,
    balance_id: i32,
    off: &OffDays,
    now: NaiveDateTime,
) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;
    lock_balance(&mut tx, balance_id).await?;
    let fresh: HolidayLeaveDeduction = sqlx::query_as(&format!(
        r#"SELECT {DEDUCTION_COLUMNS} FROM "holidayLeaveDeduction" d WHERE d.id = $1"#
    ))
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if fresh.status != HOLIDAY_DEDUCTION_APPLIED {
        return Ok(false);
    }
    // Libur bisa dinyalakan lagi sejak penyaringan; putuskan dari keadaan terbaru.
    let holiday = match fresh.holiday_id {
        Some(hid) => load_holiday(&mut tx, hid).await?,
        None => None,
    };
    let reloaded = off_days_covering(
        &mut tx,
        company_id,
        holiday.as_ref().map(|h| h.date),
        off,
        now,
    )
    .await?;
    let pred = reloaded.as_ref().unwrap_or(off);
    let is_off = |d: NaiveDate| pred.is_off(d);
    if !needs_reversal(
        fresh.holiday_id,
        fresh.holiday_date,
        holiday.as_ref(),
        Some(&is_off),
    ) {
        return Ok(false);
    }

    apply_balance_delta(
        &mut tx,
        fresh.balance_id,
        &BalanceDelta {
            carry_over_used: if fresh.carry_over_deducted > 0.0 {
                -fresh.carry_over_deducted
            } else {
                0.0
            },
            annual_leave_used: if fresh.annual_deducted > 0.0 {
                -fresh.annual_deducted
            } else {
                0.0
            },
            ..BalanceDelta::default()
        },
    )
    .await?;
    sqlx::query(
        r#"UPDATE "holidayLeaveDeduction" SET status = $2, "reversedAt" = $3, "updatedAt" = $3
           WHERE id = $1"#,
    )
    .bind(fresh.id)
    .bind(HOLIDAY_DEDUCTION_REVERSED)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    let reason = format!("Pembatalan potongan cuti bersama: {}", fresh.holiday_name);
    for (field, amount) in [
        ("carryOverUsed", fresh.carry_over_deducted),
        ("annualLeaveUsed", fresh.annual_deducted),
    ] {
        if amount <= 0.0 {
            continue;
        }
        insert_adjustment(
            &mut tx,
            &NewAdjustment {
                balance_id: fresh.balance_id,
                user_id: fresh.user_id,
                company_id: fresh.company_id,
                adjusted_by_id: None,
                field,
                delta: -amount,
                reason: reason.clone(),
                source: LeaveBalanceChangeSource::HOLIDAY_REVERSAL,
                leave_request_id: None,
                dedupe_key: None,
                holiday_id: fresh.holiday_id,
                intended_delta: None,
            },
        )
        .await?;
    }
    tx.commit().await?;
    Ok(true)
}

// ── Apply ────────────────────────────────────────────────────────────────────

async fn apply_pass(
    pool: &PgPool,
    company_id: i32,
    holiday_id: Option<i32>,
    time_zone: &str,
    now: NaiveDateTime,
) -> sqlx::Result<u32> {
    let today = local_now(now, time_zone).date();
    let year = chrono::Datelike::year(&today);
    let Some(year_start) = NaiveDate::from_ymd_opt(year, 1, 1) else {
        return Ok(0);
    };
    let holidays: Vec<HolidayState> = sqlx::query_as(
        r#"SELECT id, name, date, "isActive", "deductsLeave" FROM "companyHoliday"
           WHERE "companyId" = $1 AND "isActive" = true AND "deductsLeave" = true
             AND ($2::int IS NULL OR id = $2) AND date >= $3 AND date <= $4
           ORDER BY date ASC"#,
    )
    .bind(company_id)
    .bind(holiday_id)
    .bind(year_start)
    .bind(today)
    .fetch_all(pool)
    .await?;
    let (Some(first), Some(last)) = (holidays.first(), holidays.last()) else {
        return Ok(0);
    };

    // Satu query pola per pass (asOf = now): libur pemotong hanya memotong bila jatuh di hari
    // kerja pola yang berlaku pada tanggal itu (FR-045).
    let mut conn = pool.acquire().await?;
    let off = off_day_predicate(&mut conn, company_id, first.date, last.date, now).await?;
    let due: Vec<&HolidayState> = holidays.iter().filter(|h| !off.is_off(h.date)).collect();
    if due.is_empty() {
        return Ok(0);
    }

    // Keanggotaan aktif paling awal yang menentukan kelayakan.
    let memberships: Vec<(i32, NaiveDateTime)> = sqlx::query_as(
        r#"SELECT "userId", "createdAt" FROM "userCompany"
           WHERE "companyId" = $1 AND status = 1 ORDER BY id ASC"#,
    )
    .bind(company_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut seen = HashSet::new();
    let joined: Vec<(i32, NaiveDateTime)> = memberships
        .into_iter()
        .filter(|(u, _)| seen.insert(*u))
        .collect();

    let due_ids: Vec<i32> = due.iter().map(|h| h.id).collect();
    let existing: Vec<(Option<i32>, i32)> = sqlx::query_as(
        r#"SELECT "holidayId", "userId" FROM "holidayLeaveDeduction"
           WHERE "companyId" = $1 AND status = $2 AND "holidayId" = ANY($3)"#,
    )
    .bind(company_id)
    .bind(HOLIDAY_DEDUCTION_APPLIED)
    .bind(&due_ids)
    .fetch_all(&mut *conn)
    .await?;
    drop(conn);
    let applied: HashSet<(Option<i32>, i32)> = existing.into_iter().collect();

    let mut count = 0;
    for holiday in due {
        // endOfDay lokal tanggal libur: bergabung setelahnya → tidak dipotong.
        let next_day_start = holiday
            .date
            .succ_opt()
            .map_or(i64::MAX, |d| local_day_start(d, time_zone));
        for &(user_id, since) in &joined {
            if applied.contains(&(Some(holiday.id), user_id)) {
                continue;
            }
            if mz_overtime::zone::ms_of(since) >= next_day_start {
                continue;
            }
            match apply_one(
                pool, company_id, holiday.id, user_id, year, time_zone, now, &off,
            )
            .await
            {
                Ok(true) => count += 1,
                Ok(false) => {}
                Err(e) => tracing::error!(
                    "Potongan libur {} untuk user {user_id} gagal: {e}",
                    holiday.id
                ),
            }
        }
    }
    Ok(count)
}

#[allow(clippy::too_many_arguments)]
async fn apply_one(
    pool: &PgPool,
    company_id: i32,
    holiday_id: i32,
    user_id: i32,
    year: i32,
    time_zone: &str,
    now: NaiveDateTime,
    off: &OffDays,
) -> sqlx::Result<bool> {
    // Baris saldo harus ada sebelum transaksi supaya bisa dikunci di dalamnya.
    let ensured = get_or_create_balance(pool, user_id, company_id, year).await?;

    let mut tx = pool.begin().await?;
    let balance = lock_balance(&mut tx, ensured.id).await?;
    let existing: Option<String> = sqlx::query_scalar(
        r#"SELECT status FROM "holidayLeaveDeduction" WHERE "holidayId" = $1 AND "userId" = $2"#,
    )
    .bind(holiday_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    if existing.as_deref() == Some(HOLIDAY_DEDUCTION_APPLIED) {
        return Ok(false);
    }
    let Some(holiday) = load_holiday(&mut tx, holiday_id)
        .await?
        .filter(|h| h.is_active && h.deducts_leave)
    else {
        return Ok(false);
    };
    let reloaded = off_days_covering(&mut tx, company_id, Some(holiday.date), off, now).await?;
    if reloaded.as_ref().unwrap_or(off).is_off(holiday.date) {
        return Ok(false);
    }

    let policy = load_policy(&mut *tx, company_id).await?;
    let expired = policy_carry_over_expired(policy.as_ref(), year, time_zone, now);
    let amounts = BalanceAmounts::from(&balance);
    let split = split_deduction(
        carry_available(&amounts, expired),
        balance.annual_leave_total - balance.annual_leave_used,
        1.0,
        true,
    );
    let actual = round2(split.from_carry + split.from_annual);

    if split.from_carry > 0.0 || split.from_annual > 0.0 {
        apply_balance_delta(
            &mut tx,
            balance.id,
            &BalanceDelta {
                carry_over_used: split.from_carry,
                annual_leave_used: split.from_annual,
                ..BalanceDelta::default()
            },
        )
        .await?;
    }

    // Baris tetap ditulis walau potongannya 0, supaya tidak dicoba ulang tiap jam.
    sqlx::query(
        r#"INSERT INTO "holidayLeaveDeduction"
             ("companyId", "holidayId", "userId", "balanceId", "holidayName", "holidayDate",
              status, "intendedDays", "carryOverDeducted", "annualDeducted", "appliedAt",
              "reversedAt", "updatedAt")
           VALUES ($1, $2, $3, $4, $5, $6, $7, 1, $8, $9, $10, NULL, $10)
           ON CONFLICT ("holidayId", "userId") DO UPDATE SET
             "companyId" = EXCLUDED."companyId", "balanceId" = EXCLUDED."balanceId",
             "holidayName" = EXCLUDED."holidayName", "holidayDate" = EXCLUDED."holidayDate",
             status = EXCLUDED.status, "intendedDays" = EXCLUDED."intendedDays",
             "carryOverDeducted" = EXCLUDED."carryOverDeducted",
             "annualDeducted" = EXCLUDED."annualDeducted", "appliedAt" = EXCLUDED."appliedAt",
             "reversedAt" = NULL, "updatedAt" = EXCLUDED."updatedAt""#,
    )
    .bind(company_id)
    .bind(holiday_id)
    .bind(user_id)
    .bind(balance.id)
    .bind(&holiday.name)
    .bind(holiday.date)
    .bind(HOLIDAY_DEDUCTION_APPLIED)
    .bind(split.from_carry)
    .bind(split.from_annual)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    let reason = if actual < 1.0 {
        format!(
            "Cuti bersama: {} (terpotong {} dari 1 hari)",
            holiday.name,
            num_str(actual)
        )
    } else {
        format!("Cuti bersama: {}", holiday.name)
    };
    let mut first = true;
    for (field, amount) in [
        ("carryOverUsed", split.from_carry),
        ("annualLeaveUsed", split.from_annual),
    ] {
        if amount <= 0.0 {
            continue;
        }
        insert_adjustment(
            &mut tx,
            &NewAdjustment {
                balance_id: balance.id,
                user_id,
                company_id,
                adjusted_by_id: None,
                field,
                delta: amount,
                reason: reason.clone(),
                source: LeaveBalanceChangeSource::HOLIDAY_DEDUCTION,
                leave_request_id: None,
                dedupe_key: None,
                holiday_id: Some(holiday_id),
                intended_delta: first.then_some(1.0),
            },
        )
        .await?;
        first = false;
    }
    tx.commit().await?;
    Ok(true)
}

// ── Hitung ulang cuti tersetujui ─────────────────────────────────────────────

/// `recalcApprovedForDates(companyId, dateKeys, timeZone)`: menghitung ulang cuti APPROVED
/// yang menyentuh tanggal-tanggal ini setelah libur berubah. Refund mengurangi
/// `annualLeaveUsed` dulu lalu `carryOverUsed`; tagihan tambahan hanya sebesar yang tersedia.
pub async fn recalc_approved_for_dates(
    pool: &PgPool,
    company_id: i32,
    dates: &[NaiveDate],
    time_zone: &str,
) -> sqlx::Result<u32> {
    let mut keys: Vec<NaiveDate> = Vec::new();
    for d in dates {
        if !keys.contains(d) {
            keys.push(*d);
        }
    }
    if keys.is_empty() {
        return Ok(0);
    }
    let ids: Vec<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "leaveRequest" r
           WHERE r."companyId" = $1 AND r.status = 'APPROVED'
             AND EXISTS (SELECT 1 FROM unnest($2::date[]) k(d)
                         WHERE r."startDate" <= k.d AND r."endDate" >= k.d)
           ORDER BY id"#,
    )
    .bind(company_id)
    .bind(&keys)
    .fetch_all(pool)
    .await?;

    let mut recalculated = 0;
    for id in ids {
        match recalc_one(pool, company_id, id, time_zone).await {
            Ok(true) => recalculated += 1,
            Ok(false) => {}
            Err(e) => tracing::error!("Hitung ulang pengajuan {id} gagal: {e}"),
        }
    }
    Ok(recalculated)
}

#[derive(sqlx::FromRow)]
#[sqlx(rename_all = "camelCase")]
struct RecalcRequest {
    id: i32,
    user_id: i32,
    status: LeaveStatus,
    leave_type: Option<LeaveType>,
    start_date: NaiveDate,
    end_date: NaiveDate,
    start_day_type: LeaveDayType,
    end_day_type: LeaveDayType,
    total_days: f64,
    reviewed_at: Option<NaiveDateTime>,
    cfg_deduct_balance: Option<bool>,
    cfg_balance_source: Option<LeaveBalanceSource>,
}

async fn recalc_one(
    pool: &PgPool,
    company_id: i32,
    request_id: i32,
    time_zone: &str,
) -> anyhow::Result<bool> {
    let probe: Option<(i32, NaiveDate)> =
        sqlx::query_as(r#"SELECT "userId", "startDate" FROM "leaveRequest" WHERE id = $1"#)
            .bind(request_id)
            .fetch_optional(pool)
            .await?;
    let Some((user_id, start)) = probe else {
        return Ok(false);
    };
    let year = chrono::Datelike::year(&start);
    // Tanpa baris saldo tidak ada yang pernah dipotong, jadi tak ada yang dikoreksi.
    let Some(balance_row) = find_balance(pool, user_id, company_id, year).await? else {
        return Ok(false);
    };

    let mut tx = pool.begin().await?;
    let balance = lock_balance(&mut tx, balance_row.id).await?;
    let request: Option<RecalcRequest> = sqlx::query_as(
        r#"SELECT r.id, r."userId", r.status::text AS status, r."leaveType"::text AS "leaveType",
                  r."startDate", r."endDate", r."startDayType"::text AS "startDayType",
                  r."endDayType"::text AS "endDayType", r."totalDays", r."reviewedAt",
                  t."deductBalance" AS "cfgDeductBalance",
                  t."balanceSource"::text AS "cfgBalanceSource"
           FROM "leaveRequest" r LEFT JOIN "companyLeaveType" t ON t.id = r."leaveTypeId"
           WHERE r.id = $1"#,
    )
    .bind(request_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(request) = request.filter(|r| r.status == LeaveStatus::APPROVED) else {
        return Ok(false);
    };

    let index =
        load_holiday_index(&mut tx, company_id, request.start_date, request.end_date).await?;
    // FR-046: pola menurut saat cuti disetujui (reviewedAt; data lama tanpa reviewedAt → sekarang).
    let off = off_day_predicate(
        &mut tx,
        company_id,
        request.start_date,
        request.end_date,
        request.reviewed_at.unwrap_or_else(now_naive),
    )
    .await?;
    let is_off = |d: NaiveDate| off.is_off(d);
    let new_total = count_leave_days(
        &crate::day_count::to_date_key(request.start_date),
        &crate::day_count::to_date_key(request.end_date),
        request.start_day_type,
        request.end_day_type,
        &index.excluded_dates,
        Some(&is_off),
    )?
    .total_days;
    let diff = round2(new_total - request.total_days);
    if diff == 0.0 {
        return Ok(false);
    }

    let policy = load_policy(&mut *tx, company_id).await?;
    let has_cfg = request.cfg_deduct_balance.is_some();
    let is_sick = if has_cfg {
        request.cfg_balance_source == Some(LeaveBalanceSource::SICK)
    } else {
        request.leave_type == Some(LeaveType::SICK_LEAVE)
    };
    let deducts = match request.cfg_deduct_balance {
        Some(d) => d,
        None => matches!(
            request.leave_type,
            Some(LeaveType::ANNUAL_LEAVE | LeaveType::SICK_LEAVE)
        ),
    };

    let mut adjustments: Vec<(&'static str, f64)> = Vec::new();
    let mut intended: Option<f64> = None;
    let mut delta = BalanceDelta::default();
    if deducts {
        let sick_mode = resolve_sick_mode(policy.as_ref());
        if is_sick && sick_mode != LeaveSickMode::DEDUCT_ANNUAL {
            if diff < 0.0 {
                let back = (-diff).min(balance.sick_leave_used);
                if back > 0.0 {
                    delta.sick_leave_used = -back;
                    adjustments.push(("sickLeaveUsed", -back));
                }
            } else {
                // UNLIMITED: pemakaian dicatat tanpa batas jatah.
                let take = if sick_mode == LeaveSickMode::UNLIMITED {
                    diff
                } else {
                    diff.min((balance.sick_leave_total - balance.sick_leave_used).max(0.0))
                };
                if take > 0.0 {
                    delta.sick_leave_used = take;
                    adjustments.push(("sickLeaveUsed", take));
                }
                if take < diff {
                    intended = Some(diff);
                }
            }
        } else if diff < 0.0 {
            let from_annual = (-diff).min(balance.annual_leave_used);
            let from_carry = round2(-diff - from_annual).min(balance.carry_over_used);
            if from_annual > 0.0 {
                delta.annual_leave_used = -from_annual;
                adjustments.push(("annualLeaveUsed", -from_annual));
            }
            if from_carry > 0.0 {
                delta.carry_over_used = -from_carry;
                adjustments.push(("carryOverUsed", -from_carry));
            }
        } else {
            let expired = policy_carry_over_expired(policy.as_ref(), year, time_zone, now_naive());
            let split = split_deduction(
                carry_available(&BalanceAmounts::from(&balance), expired),
                balance.annual_leave_total - balance.annual_leave_used,
                diff,
                true,
            );
            if split.from_carry > 0.0 {
                delta.carry_over_used = split.from_carry;
                adjustments.push(("carryOverUsed", split.from_carry));
            }
            if split.from_annual > 0.0 {
                delta.annual_leave_used = split.from_annual;
                adjustments.push(("annualLeaveUsed", split.from_annual));
            }
            if split.shortfall > 0.0 {
                intended = Some(diff);
            }
        }
    }

    if !adjustments.is_empty() {
        apply_balance_delta(&mut tx, balance.id, &delta).await?;
    }
    sqlx::query(r#"UPDATE "leaveRequest" SET "totalDays" = $2, "updatedAt" = $3 WHERE id = $1"#)
        .bind(request.id)
        .bind(new_total)
        .bind(now_naive())
        .execute(&mut *tx)
        .await?;
    let reason = format!(
        "Hitung ulang pengajuan #{} karena perubahan hari libur",
        request.id
    );
    for (i, (field, amount)) in adjustments.into_iter().enumerate() {
        insert_adjustment(
            &mut tx,
            &NewAdjustment {
                balance_id: balance.id,
                user_id: request.user_id,
                company_id,
                adjusted_by_id: None,
                field,
                delta: amount,
                reason: reason.clone(),
                source: LeaveBalanceChangeSource::LEAVE_RECALC,
                leave_request_id: Some(request.id),
                dedupe_key: None,
                holiday_id: None,
                intended_delta: if i == 0 { intended } else { None },
            },
        )
        .await?;
    }
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        crate::day_count::parse_date_key(s).unwrap_or_default()
    }

    fn holiday(date: &str) -> HolidayState {
        HolidayState {
            id: 1,
            name: "Libur".into(),
            date: d(date),
            is_active: true,
            deducts_leave: true,
        }
    }

    // holiday-deduction.service.spec.ts › needsReversal
    #[test]
    fn needs_reversal_cases() {
        let h = holiday("2026-10-01");
        let row_date = d("2026-10-01");
        assert!(
            needs_reversal(None, row_date, Some(&h), None),
            "holidayId null"
        );
        assert!(
            needs_reversal(Some(1), row_date, None, None),
            "holiday missing"
        );
        let inactive = HolidayState {
            is_active: false,
            ..h.clone()
        };
        assert!(
            needs_reversal(Some(1), row_date, Some(&inactive), None),
            "inactive"
        );
        let off = HolidayState {
            deducts_leave: false,
            ..h.clone()
        };
        assert!(
            needs_reversal(Some(1), row_date, Some(&off), None),
            "not deducting"
        );
        assert!(
            needs_reversal(Some(1), row_date, Some(&holiday("2026-09-30")), None),
            "date moved"
        );
        assert!(
            needs_reversal(Some(1), d("2026-10-03"), Some(&holiday("2026-10-03")), None),
            "on a weekend"
        );
        assert!(
            !needs_reversal(Some(1), row_date, Some(&h), None),
            "unchanged weekday"
        );
    }

    // › needsReversal with a work-pattern predicate (Minggu saja libur)
    #[test]
    fn needs_reversal_with_pattern() {
        let sunday_off = |k: NaiveDate| chrono::Datelike::weekday(&k) == chrono::Weekday::Sun;
        let f: &dyn Fn(NaiveDate) -> bool = &sunday_off;
        let sat = holiday("2026-10-03");
        assert!(!needs_reversal(
            Some(1),
            d("2026-10-03"),
            Some(&sat),
            Some(f)
        ));
        let sun = holiday("2026-10-04");
        assert!(needs_reversal(
            Some(1),
            d("2026-10-04"),
            Some(&sun),
            Some(f)
        ));
        assert!(needs_reversal(Some(1), d("2026-10-03"), Some(&sat), None));
    }

    #[test]
    fn candidate_sql_encodes_conditions() {
        let sql = REVERSE_CANDIDATES_SQL;
        for part in [
            r#"LEFT JOIN "companyHoliday" h"#,
            "h.id IS NULL",
            r#"NOT h."isActive""#,
            r#"NOT h."deductsLeave""#,
            r#"h."date" <> d."holidayDate""#,
            r#"d."holidayId" IS NULL"#,
        ] {
            assert!(sql.contains(part), "{part}");
        }
        assert!(!sql.contains("ISODOW"));
    }
}
