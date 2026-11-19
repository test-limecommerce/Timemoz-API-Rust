//! `src/leaves/enums/leave.enums.ts`. Enum Postgres Prisma dibaca `col::text` dan ditulis
//! `$n::"LeaveStatus"` dsb. (lihat `mz_core::text_enum!`).

mz_core::text_enum! {
    /// `LeaveType` (jenis cuti lama, sebelum `companyLeaveType`).
    pub enum LeaveType {
        ANNUAL_LEAVE,
        SICK_LEAVE,
        MARRIAGE_LEAVE,
        MATERNITY_LEAVE,
        BEREAVEMENT_LEAVE,
        UNPAID_LEAVE,
    }
}

mz_core::text_enum! {
    /// `LeaveStatus`.
    pub enum LeaveStatus { PENDING, APPROVED, REJECTED, NEEDS_REVISION }
}

mz_core::text_enum! {
    /// `LeaveDayType`.
    pub enum LeaveDayType { FULL_DAY, HALF_DAY_MORNING, HALF_DAY_AFTERNOON }
}

mz_core::text_enum! {
    /// `LeaveBalanceSource` (`companyLeaveType.balanceSource`).
    pub enum LeaveBalanceSource { ANNUAL, SICK }
}

mz_core::text_enum! {
    /// `LeaveResetType`.
    pub enum LeaveResetType { ANNUAL, ANNIVERSARY }
}

mz_core::text_enum! {
    /// `LeaveEntitlementType`.
    pub enum LeaveEntitlementType { UNIFORM, TIER_BASED }
}

mz_core::text_enum! {
    /// `LeaveAccrualMode`.
    pub enum LeaveAccrualMode { UPFRONT, MONTHLY }
}

mz_core::text_enum! {
    /// `LeaveSickMode`.
    pub enum LeaveSickMode { DEDUCT_ANNUAL, UNLIMITED, LIMITED }
}

mz_core::text_enum! {
    /// `LeaveBalanceChangeSource` (`leaveBalanceAdjustment.source`).
    pub enum LeaveBalanceChangeSource {
        ADJUSTMENT,
        AUTO_DEDUCTION,
        MONTHLY_ACCRUAL,
        CARRY_OVER,
        FORFEIT,
        CARRY_OVER_EXPIRY,
        HOLIDAY_DEDUCTION,
        HOLIDAY_REVERSAL,
        LEAVE_RECALC,
    }
}

mz_core::text_enum! {
    /// `HolidayType` (`companyHoliday.type`).
    pub enum HolidayType { NATIONAL_HOLIDAY, JOINT_LEAVE }
}

/// `HOLIDAY_DEDUCTION_STATUS.APPLIED` (`holidayLeaveDeduction.status`, kolom VARCHAR).
pub const HOLIDAY_DEDUCTION_APPLIED: &str = "APPLIED";
/// `HOLIDAY_DEDUCTION_STATUS.REVERSED`.
pub const HOLIDAY_DEDUCTION_REVERSED: &str = "REVERSED";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_prisma() {
        assert_eq!(LeaveStatus::NEEDS_REVISION.as_str(), "NEEDS_REVISION");
        assert_eq!(
            "HALF_DAY_MORNING".parse(),
            Ok(LeaveDayType::HALF_DAY_MORNING)
        );
        assert_eq!(LeaveBalanceChangeSource::ALL.len(), 9);
        assert!("FIXED".parse::<LeaveEntitlementType>().is_err());
    }
}
