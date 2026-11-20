//! `HolidayDeductionService` (`leave-schedule/holiday-deduction.service.ts`): potongan cuti
//! bersama / libur pemotong cuti dan hitung ulang cuti tersetujui. Idempotensi berdasarkan
//! status baris `holidayLeaveDeduction`, bukan jam berjalan. Satu transaksi pendek per
//! karyawan; kunci `leaveBalance` selalu diambil lebih dulu.

use std::collections::{HashMap, HashSet};

use chrono::{Datelike, NaiveDate, NaiveDateTime};
use mz_core::time::now_naive;
use sqlx::{PgConnection, PgPool};

use crate::balance::{
    BalanceDelta, NewAdjustment, apply_balance_delta, find_balance_by_id, get_or_create_balance,
    get_or_create_balance_in, insert_adjustment, lock_balance,
};
use crate::calendar::{OffDays, load_holiday_index, off_day_predicate};
use crate::carry_over::recompute_next_year_carry_over;
use crate::day_count::is_sat_sun;
use crate::enums::{
    HOLIDAY_DEDUCTION_APPLIED, HOLIDAY_DEDUCTION_REVERSED, LeaveBalanceChangeSource,
    LeaveBalanceSource, LeaveDayType, LeaveSickMode, LeaveStatus, LeaveType,
};
use crate::js::{num_str, round2};
use crate::math::{BalanceAmounts, carry_available, resolve_sick_mode, split_deduction};
use crate::models::{DEDUCTION_COLUMNS, HolidayLeaveDeduction, HolidayState, LeaveBalance};
use crate::policy::{load_policy, local_day_start, local_now, policy_carry_over_expired};
use crate::segments::count_leave_days_by_year;

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
        match reverse_one(pool, company_id, id, balance_id, &off, time_zone, now).await {
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
    time_zone: &str,
    now: NaiveDateTime,
) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;
    let balance = lock_balance(&mut tx, balance_id).await?;
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
    if fresh.annual_deducted > 0.0 || fresh.carry_over_deducted > 0.0 {
        // Sebelum menulis baris potongan: semua kunci saldo (Y lalu Y+1) diambil lebih dulu.
        recompute_next_year_carry_over(
            &mut tx,
            fresh.company_id,
            fresh.user_id,
            balance.year,
            time_zone,
            Some(format!(
                "Koreksi sisa cuti dibawa: potongan {} dibatalkan",
                fresh.holiday_name
            )),
            now,
        )
        .await?;
    }
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
        // Sebelum upsert baris potongan: kunci saldo Y+1 diambil sebelum kunci baris lain.
        recompute_next_year_carry_over(
            &mut tx,
            company_id,
            user_id,
            balance.year,
            time_zone,
            Some(format!(
                "Koreksi sisa cuti dibawa: potongan {}",
                holiday.name
            )),
            now,
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
    user_id: i32,
    status: LeaveStatus,
    leave_type: Option<LeaveType>,
    start_date: NaiveDate,
    end_date: NaiveDate,
    start_day_type: LeaveDayType,
    end_day_type: LeaveDayType,
    reviewed_at: Option<NaiveDateTime>,
    cfg_deduct_balance: Option<bool>,
    cfg_balance_source: Option<LeaveBalanceSource>,
}

/// Kolom pemakaian yang bisa dikembalikan/ditagih hitung ulang.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsedField {
    Annual,
    Carry,
    Sick,
}

impl UsedField {
    /// `REFUND_ORDER`: annualLeaveUsed, carryOverUsed, sickLeaveUsed.
    const REFUND_ORDER: [Self; 3] = [Self::Annual, Self::Carry, Self::Sick];

    fn name(self) -> &'static str {
        match self {
            Self::Annual => "annualLeaveUsed",
            Self::Carry => "carryOverUsed",
            Self::Sick => "sickLeaveUsed",
        }
    }

    fn parse(field: &str) -> Option<Self> {
        Self::REFUND_ORDER.into_iter().find(|f| f.name() == field)
    }

    fn of_balance(self, b: &LeaveBalance) -> f64 {
        match self {
            Self::Annual => b.annual_leave_used,
            Self::Carry => b.carry_over_used,
            Self::Sick => b.sick_leave_used,
        }
    }
}

/// Tagihan pengajuan ini ke satu saldo per kolom.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Charged {
    annual: f64,
    carry: f64,
    sick: f64,
}

impl Charged {
    fn get(self, f: UsedField) -> f64 {
        match f {
            UsedField::Annual => self.annual,
            UsedField::Carry => self.carry,
            UsedField::Sick => self.sick,
        }
    }

    fn add(&mut self, f: UsedField, delta: f64) {
        let slot = match f {
            UsedField::Annual => &mut self.annual,
            UsedField::Carry => &mut self.carry,
            UsedField::Sick => &mut self.sick,
        };
        *slot = round2(*slot + delta);
    }
}

/// `YearPlan`: selisih satu tahun saldo. `charged = None` → cara lama tanpa rincian per tahun.
#[derive(Debug, Clone, Copy, PartialEq)]
struct YearPlan {
    year: i32,
    balance_id: i32,
    diff: f64,
    charged: Option<Charged>,
}

/// `planYearChange(...)`: refund annualLeaveUsed dulu, lalu carryOverUsed (sickLeaveUsed untuk
/// sakit terpisah); dengan riwayat per tahun tiap kolom dibatasi yang memang ditagih pengajuan
/// ini. Tagihan tambahan hanya sebesar yang tersedia; kekurangannya jadi `intendedDelta`.
fn plan_year_change(
    balance: &LeaveBalance,
    plan: &YearPlan,
    separate_sick: bool,
    sick_mode: LeaveSickMode,
    carry_over_expired: bool,
) -> (Vec<(UsedField, f64)>, Option<f64>) {
    let mut adjustments = Vec::new();
    let diff = plan.diff;
    if diff < 0.0 {
        let mut left = -diff;
        let order: &[UsedField] = match (plan.charged, separate_sick) {
            (Some(_), _) => &UsedField::REFUND_ORDER,
            (None, true) => &[UsedField::Sick],
            (None, false) => &[UsedField::Annual, UsedField::Carry],
        };
        for &field in order {
            let used = field.of_balance(balance);
            let cap = plan.charged.map_or(used, |c| c.get(field).min(used));
            let back = round2(left.min(cap.max(0.0)));
            if back <= 0.0 {
                continue;
            }
            adjustments.push((field, -back));
            left = round2(left - back);
        }
        return (adjustments, None);
    }

    if separate_sick {
        // UNLIMITED: pemakaian dicatat tanpa batas jatah.
        let take = if sick_mode == LeaveSickMode::UNLIMITED {
            diff
        } else {
            diff.min((balance.sick_leave_total - balance.sick_leave_used).max(0.0))
        };
        if take > 0.0 {
            adjustments.push((UsedField::Sick, take));
        }
        return (adjustments, (take < diff).then_some(diff));
    }

    let split = split_deduction(
        carry_available(&BalanceAmounts::from(balance), carry_over_expired),
        balance.annual_leave_total - balance.annual_leave_used,
        diff,
        true,
    );
    if split.from_carry > 0.0 {
        adjustments.push((UsedField::Carry, split.from_carry));
    }
    if split.from_annual > 0.0 {
        adjustments.push((UsedField::Annual, split.from_annual));
    }
    (adjustments, (split.shortfall > 0.0).then_some(diff))
}

/// `recalcOne(companyId, requestId, timeZone)`: menghitung ulang satu cuti tersetujui per tahun
/// kalender, sama dengan pemotongan saat disetujui (`countLeaveDaysByYear`).
///
/// Yang sudah dibebankan ke tiap tahun dibaca dari riwayat `AUTO_DEDUCTION` + `LEAVE_RECALC`
/// pengajuan ini (`balanceId` = tahunnya). Pengajuan tanpa baris AUTO_DEDUCTION (disetujui
/// sebelum riwayat itu ada) memakai cara lama: selisih `totalDays` dibebankan ke tahun mulai.
/// Cara lama juga dipakai untuk cuti lintas tahun yang seluruh tagihannya ada di saldo tahun
/// mulai (disetujui sebelum pemecahan per tahun).
///
/// Urutan kunci: baris tahun yang belum ada dibuat dulu (naik), baru saldo tahun mulai s.d.
/// tahun akhir + 1 dikunci (naik), lalu baris `leaveRequest`.
#[allow(clippy::too_many_lines)]
async fn recalc_one(
    pool: &PgPool,
    company_id: i32,
    request_id: i32,
    time_zone: &str,
) -> anyhow::Result<bool> {
    let now = now_naive();
    let mut tx = pool.begin().await?;
    // APPROVED sudah final: tanggal, jenis hari, jenis cuti, reviewedAt dan baris AUTO_DEDUCTION
    // tak berubah lagi, jadi boleh dibaca sebelum kunci. totalDays & LEAVE_RECALC dibaca ulang
    // sesudah kunci.
    let head: Option<RecalcRequest> = sqlx::query_as(
        r#"SELECT r."userId", r.status::text AS status, r."leaveType"::text AS "leaveType",
                  r."startDate", r."endDate", r."startDayType"::text AS "startDayType",
                  r."endDayType"::text AS "endDayType", r."reviewedAt",
                  t."deductBalance" AS "cfgDeductBalance",
                  t."balanceSource"::text AS "cfgBalanceSource"
           FROM "leaveRequest" r LEFT JOIN "companyLeaveType" t ON t.id = r."leaveTypeId"
           WHERE r.id = $1"#,
    )
    .bind(request_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(head) = head.filter(|r| r.status == LeaveStatus::APPROVED) else {
        return Ok(false);
    };
    let user_id = head.user_id;
    let start_year = head.start_date.year();
    let end_year = head.end_date.year();

    // Termasuk tahun akhir + 1: recompute_next_year_carry_over menguncinya, dan kunci itu harus
    // diambil sebelum baris pengajuan dikunci.
    let rows: Vec<(i32, i32)> = sqlx::query_as(
        r#"SELECT id, year FROM "leaveBalance"
           WHERE "userId" = $1 AND "companyId" = $2 AND year >= $3 AND year <= $4"#,
    )
    .bind(user_id)
    .bind(company_id)
    .bind(start_year)
    .bind(end_year + 1)
    .fetch_all(&mut *tx)
    .await?;
    let mut id_by_year: HashMap<i32, i32> = rows.into_iter().map(|(id, y)| (y, id)).collect();

    let has_cfg = head.cfg_deduct_balance.is_some();
    let is_sick = if has_cfg {
        head.cfg_balance_source == Some(LeaveBalanceSource::SICK)
    } else {
        head.leave_type == Some(LeaveType::SICK_LEAVE)
    };
    let deducts = match head.cfg_deduct_balance {
        Some(d) => d,
        None => matches!(
            head.leave_type,
            Some(LeaveType::ANNUAL_LEAVE | LeaveType::SICK_LEAVE)
        ),
    };
    // Penentu cara (per tahun vs lama), dibaca sebelum kunci; keputusannya stabil (cara lama
    // hanya menulis ke saldo tahun mulai, AUTO_DEDUCTION tak berubah).
    let prior_charges: Vec<(i32, String)> = if deducts {
        sqlx::query_as(
            r#"SELECT "balanceId", source::text FROM "leaveBalanceAdjustment"
               WHERE "leaveRequestId" = $1 AND source IN ('AUTO_DEDUCTION', 'LEAVE_RECALC')"#,
        )
        .bind(request_id)
        .fetch_all(&mut *tx)
        .await?
    } else {
        Vec::new()
    };
    let start_balance_id = id_by_year.get(&start_year).copied();
    let legacy_cross_year = end_year > start_year
        && prior_charges
            .iter()
            .all(|(b, _)| Some(*b) == start_balance_id);
    let per_year =
        deducts && !legacy_cross_year && prior_charges.iter().any(|(_, s)| s == "AUTO_DEDUCTION");
    // Cara lama: tanpa baris saldo tahun mulai tidak ada yang pernah dipotong.
    if !per_year && start_balance_id.is_none() {
        return Ok(false);
    }

    let index = load_holiday_index(&mut tx, company_id, head.start_date, head.end_date).await?;
    // FR-046: pola menurut saat cuti disetujui (reviewedAt; data lama tanpa reviewedAt → sekarang).
    let off = off_day_predicate(
        &mut tx,
        company_id,
        head.start_date,
        head.end_date,
        head.reviewed_at.unwrap_or(now),
    )
    .await?;
    let is_off = |d: NaiveDate| off.is_off(d);
    let (count, segments) = count_leave_days_by_year(
        &crate::day_count::to_date_key(head.start_date),
        &crate::day_count::to_date_key(head.end_date),
        head.start_day_type,
        head.end_day_type,
        &index.excluded_dates,
        Some(&is_off),
    )?;
    let new_total = count.total_days;
    let days_of = |year: i32| {
        segments
            .iter()
            .find(|s| s.year == year)
            .map_or(0.0, |s| s.days)
    };

    // Baris yang belum ada dibuat dulu (hanya bila tahun itu harus ditagih), SEBELUM kunci saldo
    // mana pun: create_balance_in_tx mengambil advisory lock tahun itu lalu mengunci Y-1.
    if per_year {
        for year in start_year..=end_year {
            if id_by_year.contains_key(&year) || days_of(year) <= 0.0 {
                continue;
            }
            let created = get_or_create_balance_in(&mut tx, user_id, company_id, year).await?;
            id_by_year.insert(year, created.id);
        }
    }
    // Lalu kunci saldo naik per tahun.
    let mut balance_id_by_year: HashMap<i32, i32> = HashMap::new();
    for year in start_year..=end_year + 1 {
        let Some(id) = id_by_year.get(&year).copied() else {
            continue;
        };
        lock_balance(&mut tx, id).await?;
        balance_id_by_year.insert(year, id);
    }
    // Dua hitung ulang pengajuan yang sama (replika berbeda) bisa mengunci tahun berbeda; kunci
    // baris pengajuan menyerialkan keduanya. Diambil SESUDAH kunci saldo.
    let current_total: Option<f64> =
        sqlx::query_scalar(r#"SELECT "totalDays" FROM "leaveRequest" WHERE id = $1 FOR UPDATE"#)
            .bind(request_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(current_total) = current_total else {
        return Ok(false);
    };

    let mut plans: Vec<YearPlan> = Vec::new();
    if per_year {
        let history: Vec<(i32, String, f64)> = sqlx::query_as(
            r#"SELECT "balanceId", field, delta FROM "leaveBalanceAdjustment"
               WHERE "leaveRequestId" = $1 AND source IN ('AUTO_DEDUCTION', 'LEAVE_RECALC')"#,
        )
        .bind(request_id)
        .fetch_all(&mut *tx)
        .await?;
        for seg in &segments {
            // Tanpa baris saldo: tak pernah ditagih dan tak ada yang harus ditagih (0 hari).
            let Some(balance_id) = balance_id_by_year.get(&seg.year).copied() else {
                continue;
            };
            let mut charged = Charged::default();
            for (b, field, delta) in &history {
                if *b != balance_id {
                    continue;
                }
                if let Some(f) = UsedField::parse(field) {
                    charged.add(f, *delta);
                }
            }
            let already = round2(charged.annual + charged.carry + charged.sick);
            plans.push(YearPlan {
                year: seg.year,
                balance_id,
                diff: round2(seg.days - already),
                charged: Some(charged),
            });
        }
    } else if deducts && let Some(balance_id) = balance_id_by_year.get(&start_year).copied() {
        plans.push(YearPlan {
            year: start_year,
            balance_id,
            diff: round2(new_total - current_total),
            charged: None,
        });
    }

    let policy = load_policy(&mut *tx, company_id).await?;
    let sick_mode = resolve_sick_mode(policy.as_ref());
    let separate_sick = is_sick && sick_mode != LeaveSickMode::DEDUCT_ANNUAL;
    let spans_years = end_year > start_year;
    let mut balance_changed = false;

    for plan in &plans {
        if plan.diff == 0.0 {
            continue;
        }
        // Baca ulang: koreksi bawaan dari tahun sebelumnya bisa mengubah baris ini.
        let balance = find_balance_by_id(&mut *tx, plan.balance_id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?;
        let expired = policy_carry_over_expired(policy.as_ref(), plan.year, time_zone, now);
        let (adjustments, intended) =
            plan_year_change(&balance, plan, separate_sick, sick_mode, expired);
        if adjustments.is_empty() {
            continue;
        }

        let mut delta = BalanceDelta::default();
        for (field, amount) in &adjustments {
            match field {
                UsedField::Annual => delta.annual_leave_used += amount,
                UsedField::Carry => delta.carry_over_used += amount,
                UsedField::Sick => delta.sick_leave_used += amount,
            }
        }
        apply_balance_delta(&mut tx, plan.balance_id, &delta).await?;
        if adjustments.iter().any(|(f, _)| *f != UsedField::Sick) {
            recompute_next_year_carry_over(
                &mut tx,
                company_id,
                user_id,
                plan.year,
                time_zone,
                Some(format!(
                    "Koreksi sisa cuti dibawa: pengajuan #{request_id} dihitung ulang"
                )),
                now,
            )
            .await?;
        }

        let reason = format!(
            "Hitung ulang pengajuan #{request_id} karena perubahan hari libur{}",
            if spans_years {
                format!(" (saldo {})", plan.year)
            } else {
                String::new()
            }
        );
        for (i, (field, amount)) in adjustments.into_iter().enumerate() {
            insert_adjustment(
                &mut tx,
                &NewAdjustment {
                    balance_id: plan.balance_id,
                    user_id,
                    company_id,
                    adjusted_by_id: None,
                    field: field.name(),
                    delta: amount,
                    reason: reason.clone(),
                    source: LeaveBalanceChangeSource::LEAVE_RECALC,
                    leave_request_id: Some(request_id),
                    dedupe_key: None,
                    holiday_id: None,
                    intended_delta: if i == 0 { intended } else { None },
                },
            )
            .await?;
        }
        balance_changed = true;
    }

    if !balance_changed && new_total == current_total {
        // Seperti Prisma `$transaction` yang mengembalikan false: baris saldo yang baru dibuat
        // tetap disimpan.
        tx.commit().await?;
        return Ok(false);
    }
    sqlx::query(r#"UPDATE "leaveRequest" SET "totalDays" = $2, "updatedAt" = $3 WHERE id = $1"#)
        .bind(request_id)
        .bind(new_total)
        .bind(now_naive())
        .execute(&mut *tx)
        .await?;
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
