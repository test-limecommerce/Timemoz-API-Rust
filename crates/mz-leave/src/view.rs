//! Cadangan cuti bersama + tampilan saldo: `loadScheduledHolidays`,
//! `getScheduledHolidayDeductions`, `toBalanceView` (`leave-policies.service.ts:695-800`).

use chrono::{NaiveDate, NaiveDateTime};
use serde::Serialize;
use sqlx::PgConnection;

use crate::calendar::off_day_predicate;
use crate::day_count::to_date_key;
use crate::enums::{HOLIDAY_DEDUCTION_APPLIED, LeaveAccrualMode};
use crate::js::ser_f64;
use crate::math::{
    BalanceAmounts, available, carry_available, requestable, resolve_annual_entitlement,
};
use crate::models::{LeaveBalance, LeavePolicy};
use crate::policy::{local_now, policy_carry_over_expired};

/// `{ holidayId, name, date, days: 1 }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledHoliday {
    pub holiday_id: i32,
    pub name: String,
    /// `YYYY-MM-DD`.
    pub date: String,
    pub days: u8,
}

/// `loadScheduledHolidays(companyId, year, timeZone, now, db)`: libur pemotong cuti yang
/// belum tiba (aktif, `deductsLeave`, hari kerja menurut pola kerja yang berlaku pada
/// tanggal itu, tanggal lokal SETELAH hari ini dan di dalam `year`). Belum memperhitungkan
/// potongan yang sudah diterapkan per karyawan.
pub async fn load_scheduled_holidays(
    conn: &mut PgConnection,
    company_id: i32,
    year: i32,
    time_zone: &str,
    now: NaiveDateTime,
) -> sqlx::Result<Vec<ScheduledHoliday>> {
    let today = local_now(now, time_zone).date();
    let (Some(year_start), Some(to)) = (
        NaiveDate::from_ymd_opt(year, 1, 1),
        NaiveDate::from_ymd_opt(year, 12, 31),
    ) else {
        return Ok(Vec::new());
    };
    // Eksklusif terhadap hari ini; untuk tahun depan batas bawahnya awal tahun itu.
    let from = if today >= year_start {
        today
    } else {
        year_start
    };
    if from > to {
        return Ok(Vec::new());
    }
    let rows: Vec<(i32, String, NaiveDate)> = sqlx::query_as(
        r#"SELECT id, name, date FROM "companyHoliday"
           WHERE "companyId" = $1 AND "isActive" = true AND "deductsLeave" = true
             AND date >= $2 AND date <= $3
           ORDER BY date ASC"#,
    )
    .bind(company_id)
    .bind(from)
    .bind(to)
    .fetch_all(&mut *conn)
    .await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let off = off_day_predicate(conn, company_id, from, to, now).await?;
    Ok(rows
        .into_iter()
        .filter(|(_, _, date)| today < year_start || *date > today)
        .filter(|(_, _, date)| !off.is_off(*date))
        .map(|(holiday_id, name, date)| ScheduledHoliday {
            holiday_id,
            name,
            date: to_date_key(date),
            days: 1,
        })
        .collect())
}

/// `getScheduledHolidayDeductions(userId, companyId, year, timeZone, now, db)`: daftar di
/// atas tanpa libur yang sudah punya potongan `APPLIED` untuk karyawan ini.
pub async fn get_scheduled_holiday_deductions(
    conn: &mut PgConnection,
    user_id: i32,
    company_id: i32,
    year: i32,
    time_zone: &str,
    now: NaiveDateTime,
) -> sqlx::Result<Vec<ScheduledHoliday>> {
    let list = load_scheduled_holidays(&mut *conn, company_id, year, time_zone, now).await?;
    if list.is_empty() {
        return Ok(list);
    }
    let ids: Vec<i32> = list.iter().map(|h| h.holiday_id).collect();
    let done: Vec<i32> = sqlx::query_scalar(
        r#"SELECT "holidayId" FROM "holidayLeaveDeduction"
           WHERE "companyId" = $1 AND "userId" = $2 AND status = $3
             AND "holidayId" = ANY($4) AND "holidayId" IS NOT NULL"#,
    )
    .bind(company_id)
    .bind(user_id)
    .bind(HOLIDAY_DEDUCTION_APPLIED)
    .bind(&ids)
    .fetch_all(conn)
    .await?;
    Ok(list
        .into_iter()
        .filter(|h| !done.contains(&h.holiday_id))
        .collect())
}

/// Konteks `toBalanceView`.
#[derive(Debug, Clone, Copy)]
pub struct BalanceViewCtx<'a> {
    pub policy: Option<&'a LeavePolicy>,
    /// `employeeLeavePolicy.annualLeaveDays`.
    pub override_days: Option<i32>,
    /// `leaveTier.annualLeaveDays` tier terpasang.
    pub tier_days: Option<i32>,
    pub time_zone: &'a str,
    pub scheduled_holiday_days: usize,
    /// "Sekarang" untuk `isCarryOverExpired` (Node: `new Date()`).
    pub now: NaiveDateTime,
}

/// `{ ...balance, accrualMode, entitlement, scheduledHolidayDays, carryOverAvailable,
/// requestable }`. `B` = bentuk saldo yang ditampilkan (mis. [`LeaveBalance`], atau struct
/// pemanggil dengan `user` ter-include) — di-`flatten` di depan.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BalanceView<B> {
    #[serde(flatten)]
    pub balance: B,
    pub accrual_mode: LeaveAccrualMode,
    #[serde(serialize_with = "ser_f64")]
    pub entitlement: f64,
    pub scheduled_holiday_days: usize,
    #[serde(serialize_with = "ser_f64")]
    pub carry_over_available: f64,
    #[serde(serialize_with = "ser_f64")]
    pub requestable: f64,
}

/// `toBalanceView(balance, ctx)` untuk [`LeaveBalance`] apa adanya.
pub fn to_balance_view(
    balance: &LeaveBalance,
    ctx: &BalanceViewCtx<'_>,
) -> BalanceView<LeaveBalance> {
    to_balance_view_with(balance.clone(), balance, ctx)
}

/// `toBalanceView` dengan bentuk tampilan `display` (angka diambil dari `balance`).
pub fn to_balance_view_with<B>(
    display: B,
    balance: &LeaveBalance,
    ctx: &BalanceViewCtx<'_>,
) -> BalanceView<B> {
    let expired = policy_carry_over_expired(ctx.policy, balance.year, ctx.time_zone, ctx.now);
    let amounts = BalanceAmounts::from(balance);
    BalanceView {
        balance: display,
        accrual_mode: ctx
            .policy
            .map_or(LeaveAccrualMode::UPFRONT, |p| p.accrual_mode),
        entitlement: resolve_annual_entitlement(ctx.policy, ctx.override_days, ctx.tier_days),
        scheduled_holiday_days: ctx.scheduled_holiday_days,
        carry_over_available: carry_available(&amounts, expired),
        requestable: requestable(
            available(&amounts, expired),
            ctx.scheduled_holiday_days as f64,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mz_core::time::JsDate;

    fn balance() -> LeaveBalance {
        LeaveBalance {
            id: 3,
            user_id: 10,
            company_id: 1,
            year: 2026,
            annual_leave_total: 12.0,
            annual_leave_used: 2.5,
            sick_leave_total: 0.0,
            sick_leave_used: 0.0,
            carry_over_days: 3.0,
            carry_over_used: 1.0,
            accrued_through_month: 0,
            created_at: JsDate(
                mz_core::time::parse_js_date("2026-01-01T00:00:00.000Z").unwrap_or_default(),
            ),
            updated_at: None,
        }
    }

    #[test]
    fn view_shape_and_order() {
        let mut policy = crate::math::tests::policy();
        policy.carry_over_expiry_month = Some(3);
        let ctx = BalanceViewCtx {
            policy: Some(&policy),
            override_days: Some(14),
            tier_days: None,
            time_zone: "Asia/Jakarta",
            scheduled_holiday_days: 2,
            now: mz_core::time::parse_js_date("2026-02-01T00:00:00Z").unwrap_or_default(),
        };
        let v = serde_json::to_string(&to_balance_view(&balance(), &ctx)).unwrap_or_default();
        assert_eq!(
            v,
            r#"{"id":3,"userId":10,"companyId":1,"year":2026,"annualLeaveTotal":12,"annualLeaveUsed":2.5,"sickLeaveTotal":0,"sickLeaveUsed":0,"carryOverDays":3,"carryOverUsed":1,"accruedThroughMonth":0,"createdAt":"2026-01-01T00:00:00.000Z","updatedAt":null,"accrualMode":"UPFRONT","entitlement":14,"scheduledHolidayDays":2,"carryOverAvailable":2,"requestable":9.5}"#
        );
        // Sesudah 1 Maret (WIB) bawaan hangus.
        let later = BalanceViewCtx {
            now: mz_core::time::parse_js_date("2026-02-28T17:00:00Z").unwrap_or_default(),
            ..ctx
        };
        let v = to_balance_view(&balance(), &later);
        assert_eq!((v.carry_over_available, v.requestable), (0.0, 7.5));
    }
}
