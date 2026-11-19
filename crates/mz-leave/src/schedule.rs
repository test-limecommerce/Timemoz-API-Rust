//! Jadwal saldo cuti per company:
//! - `LeaveAccrualService.catchUp` (`leave-schedule/leave-accrual.service.ts`)
//! - `LeaveYearService.ensureYearOpen` / `expireCarryOver` (`leave-year.service.ts`)
//! - `LeavePolicySwitchService.topUpToUpfront` (`leave-policy-switch.service.ts`)
//!
//! Satu transaksi pendek per langkah (per saldo / per bulan); galat per item dicatat lalu
//! dilewati seperti `try/catch + Sentry` Node. Idempotensi: kunci baris + cek ulang +
//! `dedupeKey` UNIQUE.

use std::collections::{HashMap, HashSet};

use chrono::{Datelike, NaiveDate, NaiveDateTime};
use mz_core::db::is_unique_violation;
use mz_core::time::now_naive;
use mz_overtime::zone::ms_of;
use sqlx::{PgConnection, PgPool};

use crate::balance::{NewAdjustment, get_or_create_balance, insert_adjustment, lock_balance};
use crate::enums::{LeaveAccrualMode, LeaveBalanceChangeSource, LeaveResetType};
use crate::js::round2;
use crate::math::{accrual_credit, pro_rata_entitlement, resolve_annual_entitlement};
use crate::policy::{
    company_time_zone, is_carry_over_expired, load_entitlement_inputs, load_policy,
    local_day_start, local_now, local_year,
};

const CHUNK_SIZE: usize = 100;

const MONTHS_ID: [&str; 12] = [
    "Januari",
    "Februari",
    "Maret",
    "April",
    "Mei",
    "Juni",
    "Juli",
    "Agustus",
    "September",
    "Oktober",
    "November",
    "Desember",
];

/// `moment({ year, month: m - 1 }).locale('id').format('MMMM YYYY')` → `Maret 2026`.
pub fn month_label_id(year: i32, month: u32) -> String {
    let name = MONTHS_ID
        .get(usize::try_from(month.saturating_sub(1)).unwrap_or(0))
        .copied()
        .unwrap_or_default();
    format!("{name} {year}")
}

/// `monthKey(year, month)` → `2026-03`.
pub fn month_key(year: i32, month: u32) -> String {
    format!("{year}-{month:02}")
}

/// Keanggotaan aktif `(userId, createdAt)` urut `id` naik.
async fn memberships(
    conn: &mut PgConnection,
    company_id: i32,
    user_ids: Option<&[i32]>,
) -> sqlx::Result<Vec<(i32, NaiveDateTime)>> {
    sqlx::query_as(
        r#"SELECT "userId", "createdAt" FROM "userCompany"
           WHERE "companyId" = $1 AND status = 1 AND ($2::int[] IS NULL OR "userId" = ANY($2))
           ORDER BY id ASC"#,
    )
    .bind(company_id)
    .bind(user_ids)
    .fetch_all(conn)
    .await
}

/// Keanggotaan aktif PALING AWAL per user, urutan kemunculan pertama (Map JS).
fn earliest_by_user(rows: &[(i32, NaiveDateTime)]) -> Vec<(i32, NaiveDateTime)> {
    let mut seen = HashSet::new();
    rows.iter()
        .filter(|(u, _)| seen.insert(*u))
        .copied()
        .collect()
}

// ── Akrual bulanan ───────────────────────────────────────────────────────────

/// `catchUp(companyId, timeZone, now)`: bulan M dikreditkan pada tick pertama saat tanggal
/// lokal company sudah di bulan M+1 atau sesudahnya. Mengembalikan jumlah kredit (> 0).
pub async fn accrual_catch_up(
    pool: &PgPool,
    company_id: i32,
    time_zone: &str,
    now: NaiveDateTime,
) -> sqlx::Result<u32> {
    let policy = load_policy(pool, company_id).await?;
    let Some(policy) = policy.filter(|p| p.accrual_mode == LeaveAccrualMode::MONTHLY) else {
        return Ok(0);
    };
    let local = local_now(now, time_zone);
    let local_year = local.year();
    let last_due_month = i32::try_from(local.month()).unwrap_or(1) - 1;
    if last_due_month < 1 {
        return Ok(0);
    }

    let balances: Vec<(i32, i32, i32)> = sqlx::query_as(
        r#"SELECT id, "userId", "accruedThroughMonth" FROM "leaveBalance"
           WHERE "companyId" = $1 AND year = $2 AND "accruedThroughMonth" < $3
           ORDER BY id"#,
    )
    .bind(company_id)
    .bind(local_year)
    .bind(last_due_month)
    .fetch_all(pool)
    .await?;
    if balances.is_empty() {
        return Ok(0);
    }

    let user_ids: Vec<i32> = balances.iter().map(|b| b.1).collect();
    let mut conn = pool.acquire().await?;
    let inputs = load_entitlement_inputs(&mut conn, company_id, &user_ids).await?;
    let joined_by_user: HashMap<i32, NaiveDateTime> =
        earliest_by_user(&memberships(&mut conn, company_id, Some(&user_ids)).await?)
            .into_iter()
            .collect();
    drop(conn);

    let monthly = policy.monthly_accrual_days.unwrap_or(0.0);
    let mut credited = 0;
    for (balance_id, user_id, accrued) in balances {
        let joined_at = joined_by_user.get(&user_id).copied();
        let entitlement = resolve_annual_entitlement(
            Some(&policy),
            inputs.override_of(user_id),
            inputs.tier_days_of(user_id),
        );
        for m in (accrued + 1)..=last_due_month {
            let month = u32::try_from(m).unwrap_or(1);
            let res = accrue_month(
                pool,
                balance_id,
                (local_year, month),
                time_zone,
                joined_at,
                monthly,
                entitlement,
            )
            .await;
            match res {
                Ok(true) => credited += 1,
                Ok(false) => {}
                Err(e) if is_unique_violation(&e) => {
                    // Bulan ini sudah dikreditkan; majukan penanda lalu lanjut.
                    advance_to(pool, balance_id, m).await?;
                }
                Err(e) => {
                    tracing::error!("catchUp gagal untuk balance {balance_id} bulan {m}: {e}");
                    break; // jangan lompati bulan; coba lagi pada tick berikutnya
                }
            }
        }
    }
    Ok(credited)
}

async fn accrue_month(
    pool: &PgPool,
    balance_id: i32,
    (year, month): (i32, u32),
    time_zone: &str,
    joined_at: Option<NaiveDateTime>,
    monthly: f64,
    entitlement: f64,
) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;
    let row = lock_balance(&mut tx, balance_id).await?;
    if i64::from(row.accrued_through_month) != i64::from(month) - 1 {
        return Ok(false);
    }
    // monthEnd = akhir bulan lokal; layak bila bergabung ≤ monthEnd (< awal bulan berikutnya).
    let next_month_start = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    };
    let eligible = match (joined_at, next_month_start) {
        (Some(j), Some(next)) => ms_of(j) < local_day_start(next, time_zone),
        _ => false,
    };
    let credit = if eligible {
        accrual_credit(monthly, entitlement, row.annual_leave_total)
    } else {
        0.0
    };

    sqlx::query(
        r#"UPDATE "leaveBalance"
           SET "annualLeaveTotal" = "annualLeaveTotal" + $2, "accruedThroughMonth" = $3,
               "updatedAt" = $4
           WHERE id = $1"#,
    )
    .bind(balance_id)
    .bind(if credit > 0.0 { credit } else { 0.0 })
    .bind(i32::try_from(month).unwrap_or(0))
    .bind(now_naive())
    .execute(&mut *tx)
    .await?;
    let done = credit > 0.0;
    if done {
        insert_adjustment(
            &mut tx,
            &NewAdjustment {
                dedupe_key: Some(format!("accrual:{balance_id}:{}", month_key(year, month))),
                ..NewAdjustment::system(
                    &row,
                    "annualLeaveTotal",
                    credit,
                    format!("Akrual bulanan — {}", month_label_id(year, month)),
                    LeaveBalanceChangeSource::MONTHLY_ACCRUAL,
                )
            },
        )
        .await?;
    }
    tx.commit().await?;
    Ok(done)
}

/// `advanceTo(balanceId, month)`: memajukan penanda tanpa memundurkannya.
async fn advance_to(pool: &PgPool, balance_id: i32, month: i32) -> sqlx::Result<()> {
    sqlx::query(
        r#"UPDATE "leaveBalance" SET "accruedThroughMonth" = $2, "updatedAt" = $3
           WHERE id = $1 AND "accruedThroughMonth" < $2"#,
    )
    .bind(balance_id)
    .bind(month)
    .bind(now_naive())
    .execute(pool)
    .await?;
    Ok(())
}

// ── Tahun cuti ───────────────────────────────────────────────────────────────

/// `ensureYearOpen(companyId, timeZone, now)`: semua mode kebijakan — setiap anggota aktif
/// wajib punya saldo tahun lokal berjalan (dibuat lewat [`get_or_create_balance`], yang ikut
/// menulis riwayat bawa/hangus). Mengembalikan jumlah saldo yang dibuat.
pub async fn ensure_year_open(
    pool: &PgPool,
    company_id: i32,
    time_zone: &str,
    now: NaiveDateTime,
) -> sqlx::Result<u32> {
    let year = local_year(now, time_zone);
    let user_ids: Vec<i32> = sqlx::query_scalar(
        r#"SELECT DISTINCT "userId" FROM "userCompany"
           WHERE "companyId" = $1 AND status = 1 ORDER BY "userId""#,
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;

    let mut created = 0;
    for chunk in user_ids.chunks(CHUNK_SIZE) {
        let have: HashSet<i32> = sqlx::query_scalar::<_, i32>(
            r#"SELECT "userId" FROM "leaveBalance"
               WHERE "companyId" = $1 AND year = $2 AND "userId" = ANY($3)"#,
        )
        .bind(company_id)
        .bind(year)
        .bind(chunk)
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect();
        for &user_id in chunk {
            if have.contains(&user_id) {
                continue;
            }
            match get_or_create_balance(pool, user_id, company_id, year).await {
                Ok(_) => created += 1,
                Err(e) => tracing::error!(
                    "ensureYearOpen gagal untuk user {user_id} company {company_id}: {e}"
                ),
            }
        }
    }
    Ok(created)
}

/// `expireCarryOver(companyId, timeZone, now)`: setelah tanggal 1 `carryOverExpiryMonth`
/// (zona company), sisa cuti bawaan dihanguskan dan dicatat `CARRY_OVER_EXPIRY`.
pub async fn expire_carry_over(
    pool: &PgPool,
    company_id: i32,
    time_zone: &str,
    now: NaiveDateTime,
) -> sqlx::Result<u32> {
    let expiry_month: Option<i32> = sqlx::query_scalar::<_, Option<i32>>(
        r#"SELECT "carryOverExpiryMonth" FROM "leavePolicy" WHERE "companyId" = $1"#,
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    let Some(month) = expiry_month.filter(|m| *m != 0) else {
        return Ok(0);
    };
    let year = local_year(now, time_zone);
    if !is_carry_over_expired(Some(month), year, time_zone, now) {
        return Ok(0);
    }
    let candidates: Vec<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "leaveBalance"
           WHERE "companyId" = $1 AND year = $2 AND "carryOverDays" > "carryOverUsed""#,
    )
    .bind(company_id)
    .bind(year)
    .fetch_all(pool)
    .await?;

    let mut expired = 0;
    for balance_id in candidates {
        match expire_one(pool, company_id, balance_id).await {
            Ok(true) => expired += 1,
            Ok(false) => {}
            Err(e) if is_unique_violation(&e) => {
                tracing::warn!(
                    "expireCarryOver: balance {balance_id} sudah punya catatan kedaluwarsa"
                );
            }
            Err(e) => tracing::error!("expireCarryOver gagal untuk balance {balance_id}: {e}"),
        }
    }
    Ok(expired)
}

async fn expire_one(pool: &PgPool, company_id: i32, balance_id: i32) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;
    let row = lock_balance(&mut tx, balance_id).await?;
    let remaining = round2(row.carry_over_days - row.carry_over_used);
    if remaining <= 0.0 {
        return Ok(false); // sudah diproses eksekusi lain
    }
    sqlx::query(
        r#"UPDATE "leaveBalance" SET "carryOverDays" = $2, "updatedAt" = $3 WHERE id = $1"#,
    )
    .bind(balance_id)
    .bind(row.carry_over_used)
    .bind(now_naive())
    .execute(&mut *tx)
    .await?;
    insert_adjustment(
        &mut tx,
        &NewAdjustment {
            company_id,
            dedupe_key: Some(format!("co-expiry:{balance_id}")),
            ..NewAdjustment::system(
                &row,
                "carryOverDays",
                -remaining,
                "Cuti bawaan kedaluwarsa",
                LeaveBalanceChangeSource::CARRY_OVER_EXPIRY,
            )
        },
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}

// ── Peralihan MONTHLY → UPFRONT ──────────────────────────────────────────────

/// `topUpToUpfront(companyId)`: saldo tahun berjalan yang di bawah jatah penuh dinaikkan ke
/// jatah penuh (pro-rata untuk tahun bergabung bila reset 1 Jan). `dedupeKey`
/// `upfront-topup:<balance>:<policy.updatedAt ms>` membuat job aman diulang. Dibatalkan bila
/// mode sudah kembali ke MONTHLY sebelum job jalan.
pub async fn top_up_to_upfront(
    pool: &PgPool,
    company_id: i32,
    now: NaiveDateTime,
) -> sqlx::Result<u32> {
    let time_zone = company_time_zone(pool, company_id).await?;
    let policy = load_policy(pool, company_id).await?;
    let (Some(time_zone), Some(policy)) = (time_zone, policy) else {
        return Ok(0);
    };
    if policy.accrual_mode != LeaveAccrualMode::UPFRONT {
        return Ok(0);
    }
    let year = local_year(now, &time_zone);
    let policy_updated_at_ms = ms_of(policy.updated_at.unwrap_or(policy.created_at).0);

    let balances: Vec<(i32, i32)> = sqlx::query_as(
        r#"SELECT id, "userId" FROM "leaveBalance" WHERE "companyId" = $1 AND year = $2 ORDER BY id"#,
    )
    .bind(company_id)
    .bind(year)
    .fetch_all(pool)
    .await?;
    if balances.is_empty() {
        return Ok(0);
    }
    let user_ids: Vec<i32> = balances.iter().map(|b| b.1).collect();
    let mut conn = pool.acquire().await?;
    let inputs = load_entitlement_inputs(&mut conn, company_id, &user_ids).await?;
    // Urut naik: keanggotaan TERBARU menang, sama dengan `joinMonthInYear`.
    let joined_by_user: HashMap<i32, NaiveDateTime> =
        memberships(&mut conn, company_id, Some(&user_ids))
            .await?
            .into_iter()
            .collect();
    drop(conn);
    let is_calendar_reset = policy.reset_type == LeaveResetType::ANNUAL;

    let mut topped_up = 0;
    for (balance_id, user_id) in balances {
        let mut entitlement = resolve_annual_entitlement(
            Some(&policy),
            inputs.override_of(user_id),
            inputs.tier_days_of(user_id),
        );
        if let Some(j) = joined_by_user.get(&user_id) {
            let joined = local_now(*j, &time_zone);
            if is_calendar_reset && joined.year() == year {
                entitlement = pro_rata_entitlement(entitlement, joined.month0());
            }
        }
        match top_up_one(pool, balance_id, entitlement, policy_updated_at_ms).await {
            Ok(true) => topped_up += 1,
            Ok(false) => {}
            Err(e) if is_unique_violation(&e) => {}
            Err(e) => tracing::error!("topUpToUpfront gagal untuk balance {balance_id}: {e}"),
        }
    }
    Ok(topped_up)
}

async fn top_up_one(
    pool: &PgPool,
    balance_id: i32,
    entitlement: f64,
    policy_updated_at_ms: i64,
) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;
    let row = lock_balance(&mut tx, balance_id).await?;
    let delta = round2(entitlement - row.annual_leave_total);
    if delta <= 0.0 {
        return Ok(false);
    }
    sqlx::query(
        r#"UPDATE "leaveBalance" SET "annualLeaveTotal" = "annualLeaveTotal" + $2, "updatedAt" = $3
           WHERE id = $1"#,
    )
    .bind(balance_id)
    .bind(delta)
    .bind(now_naive())
    .execute(&mut *tx)
    .await?;
    insert_adjustment(
        &mut tx,
        &NewAdjustment {
            dedupe_key: Some(format!("upfront-topup:{balance_id}:{policy_updated_at_ms}")),
            ..NewAdjustment::system(
                &row,
                "annualLeaveTotal",
                delta,
                "Beralih ke jatah penuh",
                LeaveBalanceChangeSource::MONTHLY_ACCRUAL,
            )
        },
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(month_label_id(2026, 1), "Januari 2026");
        assert_eq!(month_label_id(2026, 8), "Agustus 2026");
        assert_eq!(month_key(2026, 3), "2026-03");
    }

    #[test]
    fn earliest_membership_wins() {
        let t = |s: &str| mz_core::time::parse_js_date(s).unwrap_or_default();
        let rows = vec![
            (5, t("2025-01-01T00:00:00Z")),
            (6, t("2025-02-01T00:00:00Z")),
            (5, t("2026-01-01T00:00:00Z")),
        ];
        assert_eq!(
            earliest_by_user(&rows),
            vec![
                (5, t("2025-01-01T00:00:00Z")),
                (6, t("2025-02-01T00:00:00Z"))
            ]
        );
        // Map JS dari urutan naik: yang terakhir menang.
        let last: HashMap<i32, NaiveDateTime> = rows.into_iter().collect();
        assert_eq!(last.get(&5), Some(&t("2026-01-01T00:00:00Z")));
    }
}
