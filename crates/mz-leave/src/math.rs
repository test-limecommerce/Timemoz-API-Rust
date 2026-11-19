//! `src/leaves/leave-balance-math.ts` (murni) + `resolveSickMode` (`sick-leave-mode.ts`).

use crate::enums::{LeaveEntitlementType, LeaveSickMode};
use crate::js::round2;
use crate::models::{LeaveBalance, LeavePolicy};

/// `BalanceAmounts`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BalanceAmounts {
    pub annual_leave_total: f64,
    pub annual_leave_used: f64,
    pub carry_over_days: f64,
    pub carry_over_used: f64,
}

impl From<&LeaveBalance> for BalanceAmounts {
    fn from(b: &LeaveBalance) -> Self {
        Self {
            annual_leave_total: b.annual_leave_total,
            annual_leave_used: b.annual_leave_used,
            carry_over_days: b.carry_over_days,
            carry_over_used: b.carry_over_used,
        }
    }
}

/// `resolveAnnualEntitlement({ policy, override, tier })`: tier bila policy `TIER_BASED` dan
/// tier terpasang; selain itu override → policy → 12.
/// `override_days` = `employeeLeavePolicy.annualLeaveDays`, `tier_days` = `leaveTier.annualLeaveDays`.
pub fn resolve_annual_entitlement(
    policy: Option<&LeavePolicy>,
    override_days: Option<i32>,
    tier_days: Option<i32>,
) -> f64 {
    if let (Some(p), Some(t)) = (policy, tier_days)
        && p.entitlement_type == LeaveEntitlementType::TIER_BASED
    {
        return f64::from(t);
    }
    f64::from(
        override_days
            .or(policy.map(|p| p.annual_leave_days))
            .unwrap_or(12),
    )
}

/// `proRataEntitlement(entitlement, joinMonth)`: jatah × sisa bulan / 12 (bulan bergabung
/// dihitung penuh), dibulatkan ke bawah ke kelipatan setengah hari. `join_month0` 0-based.
pub fn pro_rata_entitlement(entitlement: f64, join_month0: u32) -> f64 {
    let months = 12.0 - f64::from(join_month0);
    ((entitlement * months) / 12.0 * 2.0).floor() / 2.0
}

/// `carryAvailable(balance, carryOverExpired)`.
pub fn carry_available(b: &BalanceAmounts, carry_over_expired: bool) -> f64 {
    if carry_over_expired {
        return 0.0;
    }
    round2(b.carry_over_days - b.carry_over_used).max(0.0)
}

/// `available(balance, carryOverExpired)`: sisa tahunan + sisa bawaan yang belum hangus.
pub fn available(b: &BalanceAmounts, carry_over_expired: bool) -> f64 {
    round2(b.annual_leave_total - b.annual_leave_used + carry_available(b, carry_over_expired))
}

/// `requestable(available, scheduledHolidayDays)`: dikurangi cadangan cuti bersama, min 0.
pub fn requestable(available_days: f64, scheduled_holiday_days: f64) -> f64 {
    round2(available_days - scheduled_holiday_days).max(0.0)
}

/// `accrualCredit({ monthlyAccrualDays, entitlement, annualLeaveTotal })`: kredit bulanan
/// yang tidak pernah mendorong total melewati jatah.
pub fn accrual_credit(monthly_accrual_days: f64, entitlement: f64, annual_leave_total: f64) -> f64 {
    let room = (entitlement - annual_leave_total).max(0.0);
    round2(monthly_accrual_days.min(room))
}

/// Hasil [`split_deduction`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Split {
    pub from_carry: f64,
    pub from_annual: f64,
    pub shortfall: f64,
}

/// `splitDeduction({ carryAvail, annualAvail, days, allowPartial })`: bawaan dulu, lalu
/// tahunan. `allow_partial` tidak mengubah angka (pemanggil yang menolak `shortfall > 0`).
pub fn split_deduction(
    carry_avail: f64,
    annual_avail: f64,
    days: f64,
    _allow_partial: bool,
) -> Split {
    let carry_avail = carry_avail.max(0.0);
    let annual_avail = annual_avail.max(0.0);
    let from_carry = round2(carry_avail.min(days));
    let remaining = round2(days - from_carry);
    let from_annual = round2(annual_avail.min(remaining));
    let shortfall = round2(remaining - from_annual);
    Split {
        from_carry,
        from_annual,
        shortfall,
    }
}

/// `resolveSickMode(policy)`: tanpa policy → `DEDUCT_ANNUAL`. Kolom `sickLeaveMode` selalu
/// terisi di DB (default `DEDUCT_ANNUAL`), jadi nilainya dipakai apa adanya.
pub fn resolve_sick_mode(policy: Option<&LeavePolicy>) -> LeaveSickMode {
    policy.map_or(LeaveSickMode::DEDUCT_ANNUAL, |p| p.sick_leave_mode)
}

/// `resolveSickMode` untuk objek mirip policy yang `sickLeaveMode`-nya bisa kosong (klien
/// lama): string mode tak dikenal/kosong → diturunkan dari `sickLeaveDays`.
pub fn resolve_sick_mode_raw(
    present: bool,
    sick_leave_mode: Option<&str>,
    sick_leave_days: Option<i32>,
) -> LeaveSickMode {
    if !present {
        return LeaveSickMode::DEDUCT_ANNUAL;
    }
    if let Some(m) = sick_leave_mode.and_then(|m| m.parse::<LeaveSickMode>().ok()) {
        return m;
    }
    if sick_leave_days.is_some() {
        LeaveSickMode::LIMITED
    } else {
        LeaveSickMode::DEDUCT_ANNUAL
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::enums::{LeaveAccrualMode, LeaveResetType};
    use mz_core::time::JsDate;

    /// Policy uji (nilai bawaan Prisma).
    pub(crate) fn policy() -> LeavePolicy {
        LeavePolicy {
            id: 1,
            company_id: 1,
            annual_leave_days: 12,
            sick_leave_days: None,
            sick_leave_mode: LeaveSickMode::DEDUCT_ANNUAL,
            carry_over_enabled: false,
            max_carry_over_days: None,
            carry_over_expiry_month: None,
            reset_type: LeaveResetType::ANNUAL,
            entitlement_type: LeaveEntitlementType::UNIFORM,
            accrual_mode: LeaveAccrualMode::UPFRONT,
            monthly_accrual_days: None,
            created_at: JsDate::now(),
            updated_at: None,
        }
    }

    fn tiered() -> LeavePolicy {
        LeavePolicy {
            entitlement_type: LeaveEntitlementType::TIER_BASED,
            ..policy()
        }
    }

    // leave-balance-math.spec.ts › resolveAnnualEntitlement
    #[test]
    fn entitlement_precedence() {
        assert_eq!(
            resolve_annual_entitlement(Some(&tiered()), Some(20), Some(15)),
            15.0
        );
        assert_eq!(
            resolve_annual_entitlement(Some(&policy()), Some(20), Some(15)),
            20.0
        );
        let p14 = LeavePolicy {
            annual_leave_days: 14,
            ..policy()
        };
        assert_eq!(resolve_annual_entitlement(Some(&p14), None, None), 14.0);
        assert_eq!(resolve_annual_entitlement(None, None, None), 12.0);
        assert_eq!(
            resolve_annual_entitlement(Some(&policy()), None, Some(15)),
            12.0
        );
        // Tier-based tanpa tier → override/policy.
        assert_eq!(
            resolve_annual_entitlement(Some(&tiered()), Some(9), None),
            9.0
        );
    }

    // › proRataEntitlement
    #[test]
    fn pro_rata() {
        assert_eq!(pro_rata_entitlement(12.0, 0), 12.0);
        assert_eq!(pro_rata_entitlement(12.0, 6), 6.0);
        assert_eq!(pro_rata_entitlement(12.0, 9), 3.0);
        assert_eq!(pro_rata_entitlement(12.0, 11), 1.0);
        assert_eq!(pro_rata_entitlement(14.0, 9), 3.5);
        assert_eq!(pro_rata_entitlement(15.0, 10), 2.5);
        assert_eq!(pro_rata_entitlement(13.0, 9), 3.0);
    }

    // › accrualCredit
    #[test]
    fn accrual() {
        assert_eq!(accrual_credit(1.0, 12.0, 11.5), 0.5);
        assert_eq!(accrual_credit(1.0, 12.0, 12.0), 0.0);
        assert_eq!(accrual_credit(1.0, 12.0, 3.0), 1.0);
        assert_eq!(accrual_credit(1.0, 12.0, 13.0), 0.0);
    }

    // › available / requestable
    #[test]
    fn available_and_requestable() {
        let bal = BalanceAmounts {
            annual_leave_total: 6.0,
            annual_leave_used: 2.0,
            carry_over_days: 3.0,
            carry_over_used: 1.0,
        };
        assert_eq!(available(&bal, false), 6.0);
        assert_eq!(available(&bal, true), 4.0);
        assert_eq!(requestable(6.0, 2.0), 4.0);
        assert_eq!(requestable(1.0, 3.0), 0.0);
        let over = BalanceAmounts {
            carry_over_days: 1.0,
            carry_over_used: 2.0,
            ..bal
        };
        assert_eq!(carry_available(&over, false), 0.0);
    }

    // › splitDeduction
    #[test]
    fn split() {
        assert_eq!(
            split_deduction(2.0, 10.0, 3.0, false),
            Split {
                from_carry: 2.0,
                from_annual: 1.0,
                shortfall: 0.0
            }
        );
        assert_eq!(split_deduction(1.0, 1.0, 3.0, false).shortfall, 1.0);
        assert_eq!(
            split_deduction(0.0, 0.5, 1.0, true),
            Split {
                from_carry: 0.0,
                from_annual: 0.5,
                shortfall: 0.5
            }
        );
        assert_eq!(split_deduction(-1.0, -2.0, 1.0, true).shortfall, 1.0);
    }

    #[test]
    fn sick_mode() {
        assert_eq!(resolve_sick_mode(None), LeaveSickMode::DEDUCT_ANNUAL);
        let p = LeavePolicy {
            sick_leave_mode: LeaveSickMode::UNLIMITED,
            ..policy()
        };
        assert_eq!(resolve_sick_mode(Some(&p)), LeaveSickMode::UNLIMITED);
        assert_eq!(
            resolve_sick_mode_raw(false, Some("LIMITED"), Some(3)),
            LeaveSickMode::DEDUCT_ANNUAL
        );
        assert_eq!(
            resolve_sick_mode_raw(true, None, Some(3)),
            LeaveSickMode::LIMITED
        );
        assert_eq!(
            resolve_sick_mode_raw(true, Some("x"), None),
            LeaveSickMode::DEDUCT_ANNUAL
        );
        assert_eq!(
            resolve_sick_mode_raw(true, Some("UNLIMITED"), None),
            LeaveSickMode::UNLIMITED
        );
    }
}
