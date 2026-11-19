//! Saldo & ledger cuti: `leave-balance-lock.ts`, `LeavePoliciesService.getOrCreateBalance`,
//! `buildAndCreateBalance`, `createBalanceInTx`, `joinMonthInYear`
//! (`leave-policies.service.ts:869-1010`), dan tulisan `leaveBalanceAdjustment`.

use chrono::{Datelike, NaiveDateTime};
use mz_core::time::now_naive;
use sqlx::{PgConnection, PgExecutor, PgPool};

use crate::enums::{LeaveAccrualMode, LeaveBalanceChangeSource, LeaveResetType, LeaveSickMode};
use crate::js::round2;
use crate::math::{pro_rata_entitlement, resolve_annual_entitlement, resolve_sick_mode};
use crate::models::{BALANCE_COLUMNS, LeaveBalance};
use crate::policy::{load_entitlement_of, load_policy, local_now, require_time_zone};

/// `leaveBalance.findUnique({ where: { userId_companyId_year } })`.
pub async fn find_balance(
    exec: impl PgExecutor<'_>,
    user_id: i32,
    company_id: i32,
    year: i32,
) -> sqlx::Result<Option<LeaveBalance>> {
    sqlx::query_as(&format!(
        r#"SELECT {BALANCE_COLUMNS} FROM "leaveBalance" b
           WHERE b."userId" = $1 AND b."companyId" = $2 AND b.year = $3"#
    ))
    .bind(user_id)
    .bind(company_id)
    .bind(year)
    .fetch_optional(exec)
    .await
}

/// `leaveBalance.findUnique({ where: { id } })`.
pub async fn find_balance_by_id(
    exec: impl PgExecutor<'_>,
    balance_id: i32,
) -> sqlx::Result<Option<LeaveBalance>> {
    sqlx::query_as(&format!(
        r#"SELECT {BALANCE_COLUMNS} FROM "leaveBalance" b WHERE b.id = $1"#
    ))
    .bind(balance_id)
    .fetch_optional(exec)
    .await
}

/// `lockBalance(tx, balanceId)`: `SELECT … FOR UPDATE` lalu baca ulang barisnya (baris
/// hilang → `RowNotFound`, setara `findUniqueOrThrow`).
///
/// Aturan urutan kunci: ambil kunci ini SEBELUM menyentuh baris `leaveRequest` atau
/// `holidayLeaveDeduction` karyawan yang sama. Selalu di dalam transaksi; putuskan dari baris
/// yang dikembalikan, bukan dari snapshot sebelum transaksi.
pub async fn lock_balance(conn: &mut PgConnection, balance_id: i32) -> sqlx::Result<LeaveBalance> {
    sqlx::query(r#"SELECT id FROM "leaveBalance" WHERE id = $1 FOR UPDATE"#)
        .bind(balance_id)
        .fetch_optional(&mut *conn)
        .await?;
    find_balance_by_id(conn, balance_id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)
}

/// Satu baris `leaveBalanceAdjustment` (`create({ data })`). `createdAt` = default DB.
#[derive(Debug, Clone, PartialEq)]
pub struct NewAdjustment {
    pub balance_id: i32,
    pub user_id: i32,
    pub company_id: i32,
    pub adjusted_by_id: Option<i32>,
    /// `annualLeaveTotal` | `annualLeaveUsed` | `sickLeaveTotal` | `sickLeaveUsed` |
    /// `carryOverDays` | `carryOverUsed`.
    pub field: &'static str,
    pub delta: f64,
    pub reason: String,
    pub source: LeaveBalanceChangeSource,
    pub leave_request_id: Option<i32>,
    pub dedupe_key: Option<String>,
    pub holiday_id: Option<i32>,
    pub intended_delta: Option<f64>,
}

impl NewAdjustment {
    /// Entri sistem (`adjustedById: null`) tanpa relasi opsional.
    pub fn system(
        balance: &LeaveBalance,
        field: &'static str,
        delta: f64,
        reason: impl Into<String>,
        source: LeaveBalanceChangeSource,
    ) -> Self {
        Self {
            balance_id: balance.id,
            user_id: balance.user_id,
            company_id: balance.company_id,
            adjusted_by_id: None,
            field,
            delta,
            reason: reason.into(),
            source,
            leave_request_id: None,
            dedupe_key: None,
            holiday_id: None,
            intended_delta: None,
        }
    }
}

const ADJ_INSERT: &str = r#"INSERT INTO "leaveBalanceAdjustment"
    ("balanceId", "userId", "companyId", "adjustedById", field, delta, reason, source,
     "leaveRequestId", "dedupeKey", "holidayId", "intendedDelta")
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8::"LeaveBalanceChangeSource", $9, $10, $11, $12)"#;

/// `leaveBalanceAdjustment.create`. `dedupeKey` ganda → unique violation (Node P2002);
/// cek dengan `mz_core::db::is_unique_violation`.
pub async fn insert_adjustment(conn: &mut PgConnection, a: &NewAdjustment) -> sqlx::Result<()> {
    insert(conn, a, false).await
}

/// `createMany({ data, skipDuplicates: true })` = `ON CONFLICT DO NOTHING` (tidak
/// menggugurkan transaksi).
pub async fn insert_adjustments_skip_duplicates(
    conn: &mut PgConnection,
    rows: &[NewAdjustment],
) -> sqlx::Result<()> {
    for a in rows {
        insert(&mut *conn, a, true).await?;
    }
    Ok(())
}

async fn insert(
    conn: &mut PgConnection,
    a: &NewAdjustment,
    skip_duplicates: bool,
) -> sqlx::Result<()> {
    let sql = if skip_duplicates {
        format!("{ADJ_INSERT} ON CONFLICT DO NOTHING")
    } else {
        ADJ_INSERT.to_owned()
    };
    sqlx::query(&sql)
        .bind(a.balance_id)
        .bind(a.user_id)
        .bind(a.company_id)
        .bind(a.adjusted_by_id)
        .bind(a.field)
        .bind(a.delta)
        .bind(&a.reason)
        .bind(a.source.as_str())
        .bind(a.leave_request_id)
        .bind(a.dedupe_key.as_deref())
        .bind(a.holiday_id)
        .bind(a.intended_delta)
        .execute(conn)
        .await?;
    Ok(())
}

/// Perubahan kolom saldo (`increment`/`decrement` Prisma) + `updatedAt`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BalanceDelta {
    pub annual_leave_total: f64,
    pub annual_leave_used: f64,
    pub sick_leave_used: f64,
    pub carry_over_days: f64,
    pub carry_over_used: f64,
}

/// `leaveBalance.update({ data: { x: { increment } , updatedAt: new Date() } })`. Kolom
/// dengan delta 0 tidak disentuh (sama dengan spread kondisional Node).
pub async fn apply_balance_delta(
    conn: &mut PgConnection,
    balance_id: i32,
    d: &BalanceDelta,
) -> sqlx::Result<()> {
    sqlx::query(
        r#"UPDATE "leaveBalance" SET
             "annualLeaveTotal" = "annualLeaveTotal" + $2,
             "annualLeaveUsed" = "annualLeaveUsed" + $3,
             "sickLeaveUsed" = "sickLeaveUsed" + $4,
             "carryOverDays" = "carryOverDays" + $5,
             "carryOverUsed" = "carryOverUsed" + $6,
             "updatedAt" = $7
           WHERE id = $1"#,
    )
    .bind(balance_id)
    .bind(d.annual_leave_total)
    .bind(d.annual_leave_used)
    .bind(d.sick_leave_used)
    .bind(d.carry_over_days)
    .bind(d.carry_over_used)
    .bind(now_naive())
    .execute(conn)
    .await?;
    Ok(())
}

/// `getOrCreateBalance(userId, companyId, year)` tanpa `tx`: baca; tidak ada → buat dalam
/// transaksi sendiri ([`create_balance`]).
pub async fn get_or_create_balance(
    pool: &PgPool,
    user_id: i32,
    company_id: i32,
    year: i32,
) -> sqlx::Result<LeaveBalance> {
    if let Some(b) = find_balance(pool, user_id, company_id, year).await? {
        return Ok(b);
    }
    create_balance(pool, user_id, company_id, year).await
}

/// `getOrCreateBalance(userId, companyId, year, tx)`: di dalam transaksi pemanggil.
pub async fn get_or_create_balance_in(
    conn: &mut PgConnection,
    user_id: i32,
    company_id: i32,
    year: i32,
) -> sqlx::Result<LeaveBalance> {
    if let Some(b) = find_balance(&mut *conn, user_id, company_id, year).await? {
        return Ok(b);
    }
    create_balance_in_tx(conn, user_id, company_id, year).await
}

/// `buildAndCreateBalance` tanpa `tx`: transaksi sendiri; unique violation (defensif) →
/// baca ulang baris yang dibuat bersamaan.
pub async fn create_balance(
    pool: &PgPool,
    user_id: i32,
    company_id: i32,
    year: i32,
) -> sqlx::Result<LeaveBalance> {
    let attempt = async {
        let mut tx = pool.begin().await?;
        let b = create_balance_in_tx(&mut tx, user_id, company_id, year).await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>(b)
    }
    .await;
    match attempt {
        Err(e) if mz_core::db::is_unique_violation(&e) => {
            match find_balance(pool, user_id, company_id, year).await? {
                Some(b) => Ok(b),
                None => Err(e),
            }
        }
        other => other,
    }
}

/// `joinMonthInYear`: bulan (0-based) keanggotaan aktif TERBARU dibuat, bila jatuh di `year`
/// menurut zona company.
async fn join_month_in_year(
    conn: &mut PgConnection,
    user_id: i32,
    company_id: i32,
    year: i32,
) -> sqlx::Result<Option<u32>> {
    let tz = require_time_zone(&mut *conn, company_id).await?;
    let joined: Option<NaiveDateTime> = sqlx::query_scalar(
        r#"SELECT "createdAt" FROM "userCompany"
           WHERE "userId" = $1 AND "companyId" = $2 AND status = 1
           ORDER BY id DESC LIMIT 1"#,
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(joined.and_then(|at| {
        let local = local_now(at, &tz);
        (local.year() == year).then(|| local.month0())
    }))
}

/// Teks alasan hangus (`FORFEIT`).
fn forfeit_reason(carry_over_enabled: bool, max_carry_over_days: Option<i32>) -> String {
    if carry_over_enabled {
        let cap = max_carry_over_days.map_or_else(|| "tanpa batas".to_owned(), |d| d.to_string());
        format!("Sisa cuti hangus (melebihi batas bawa {cap} hari)")
    } else {
        "Sisa cuti hangus (bawa sisa cuti tidak aktif)".to_owned()
    }
}

/// `createBalanceInTx(db, userId, companyId, year)`: membuat saldo tahun `year` beserta
/// riwayat bawa (`CARRY_OVER`) / hangus (`FORFEIT`) dari saldo Y-1 dalam SATU transaksi
/// (`conn` WAJIB koneksi transaksi). Advisory lock `leaveBalance:<c>:<u>:<y>` menyerialkan
/// pembuat baris yang sama; cek ulang keberadaan menggantikan P2002.
pub async fn create_balance_in_tx(
    conn: &mut PgConnection,
    user_id: i32,
    company_id: i32,
    year: i32,
) -> sqlx::Result<LeaveBalance> {
    let lock_key = format!("leaveBalance:{company_id}:{user_id}:{year}");
    mz_core::db::advisory_xact_lock(&mut *conn, &lock_key).await?;
    if let Some(b) = find_balance(&mut *conn, user_id, company_id, year).await? {
        return Ok(b);
    }

    let policy = load_policy(&mut *conn, company_id).await?;
    let (override_days, tier) = load_entitlement_of(&mut *conn, user_id, company_id).await?;
    let prev_row = find_balance(&mut *conn, user_id, company_id, year - 1).await?;

    let annual_leave_days = resolve_annual_entitlement(
        policy.as_ref(),
        override_days,
        tier.as_ref().map(|t| t.annual_leave_days),
    );
    let sick_leave_total = if resolve_sick_mode(policy.as_ref()) == LeaveSickMode::LIMITED {
        f64::from(policy.as_ref().and_then(|p| p.sick_leave_days).unwrap_or(0))
    } else {
        0.0
    };

    // Kunci baris Y-1 sebelum membacanya (urutan kunci: tahun naik).
    let prev = match prev_row {
        Some(p) => Some(lock_balance(&mut *conn, p.id).await?),
        None => None,
    };

    let carry_enabled = policy.as_ref().is_some_and(|p| p.carry_over_enabled);
    let max_carry = policy.as_ref().and_then(|p| p.max_carry_over_days);
    let mut carry_over_days = 0.0;
    if let (true, Some(p)) = (carry_enabled, prev.as_ref()) {
        let unused = p.annual_leave_total - p.annual_leave_used;
        let cap = max_carry.map_or(f64::INFINITY, f64::from);
        carry_over_days = unused.min(cap).max(0.0);
    }

    // MONTHLY: mulai 0; kredit bulan lewat diberikan catch-up akrual. UPFRONT + reset 1 Jan:
    // tahun bergabung dapat jatah pro-rata sisa bulan.
    let is_monthly = policy
        .as_ref()
        .is_some_and(|p| p.accrual_mode == LeaveAccrualMode::MONTHLY);
    let is_calendar_reset = policy
        .as_ref()
        .is_none_or(|p| p.reset_type == LeaveResetType::ANNUAL);
    let join_month = if is_monthly || is_calendar_reset {
        join_month_in_year(&mut *conn, user_id, company_id, year).await?
    } else {
        None
    };
    let accrued_through_month = if is_monthly {
        join_month.map_or(0, |m| i32::try_from(m).unwrap_or(0))
    } else {
        0
    };
    let annual_leave_total = if is_monthly {
        0.0
    } else if let Some(m) = join_month {
        pro_rata_entitlement(annual_leave_days, m)
    } else {
        annual_leave_days
    };

    let created: LeaveBalance = sqlx::query_as(&format!(
        r#"INSERT INTO "leaveBalance" AS b
             ("userId", "companyId", year, "annualLeaveTotal", "accruedThroughMonth",
              "annualLeaveUsed", "sickLeaveTotal", "sickLeaveUsed", "carryOverDays", "carryOverUsed")
           VALUES ($1, $2, $3, $4, $5, 0, $6, 0, $7, 0)
           RETURNING {BALANCE_COLUMNS}"#
    ))
    .bind(user_id)
    .bind(company_id)
    .bind(year)
    .bind(annual_leave_total)
    .bind(accrued_through_month)
    .bind(sick_leave_total)
    .bind(carry_over_days)
    .fetch_one(&mut *conn)
    .await?;

    if let Some(p) = prev {
        let unused_carry = (p.carry_over_days - p.carry_over_used).max(0.0);
        let forfeited = round2(
            (p.annual_leave_total - p.annual_leave_used - carry_over_days).max(0.0) + unused_carry,
        );
        let mut history = Vec::new();
        if carry_over_days > 0.0 {
            history.push(NewAdjustment {
                dedupe_key: Some(format!("carry:{}", created.id)),
                ..NewAdjustment::system(
                    &created,
                    "carryOverDays",
                    carry_over_days,
                    format!("Sisa cuti dibawa dari {}", year - 1),
                    LeaveBalanceChangeSource::CARRY_OVER,
                )
            });
        }
        if forfeited > 0.0 {
            // Total Y-1 TIDAK dikurangi: baris riwayat yang mencatat, tahunnya sudah ditutup.
            history.push(NewAdjustment {
                dedupe_key: Some(format!("forfeit:{}", p.id)),
                ..NewAdjustment::system(
                    &p,
                    "annualLeaveTotal",
                    -forfeited,
                    forfeit_reason(carry_enabled, max_carry),
                    LeaveBalanceChangeSource::FORFEIT,
                )
            });
        }
        insert_adjustments_skip_duplicates(conn, &history).await?;
    }
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forfeit_reasons() {
        assert_eq!(
            forfeit_reason(true, Some(6)),
            "Sisa cuti hangus (melebihi batas bawa 6 hari)"
        );
        assert_eq!(
            forfeit_reason(true, None),
            "Sisa cuti hangus (melebihi batas bawa tanpa batas hari)"
        );
        assert_eq!(
            forfeit_reason(false, Some(6)),
            "Sisa cuti hangus (bawa sisa cuti tidak aktif)"
        );
    }
}
