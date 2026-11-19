//! Baris Prisma modul cuti (SEMUA kolom skalar, urutan = `schema.prisma`, seperti hasil
//! `findUnique` tanpa `select`). Serialisasi camelCase; `DateTime` → [`JsDate`], `Float` →
//! angka JS ([`crate::js::ser_f64`]). Baca dengan konstanta `*_COLUMNS` (alias opsional).

use mz_core::time::JsDate;
use serde::Serialize;
use sqlx::FromRow;

use crate::enums::{LeaveAccrualMode, LeaveEntitlementType, LeaveResetType, LeaveSickMode};
use crate::js::{ser_f64, ser_opt_f64};

/// Model `leavePolicy`.
#[derive(Debug, Clone, PartialEq, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
#[sqlx(rename_all = "camelCase")]
pub struct LeavePolicy {
    pub id: i32,
    pub company_id: i32,
    pub annual_leave_days: i32,
    pub sick_leave_days: Option<i32>,
    pub sick_leave_mode: LeaveSickMode,
    pub carry_over_enabled: bool,
    pub max_carry_over_days: Option<i32>,
    pub carry_over_expiry_month: Option<i32>,
    pub reset_type: LeaveResetType,
    pub entitlement_type: LeaveEntitlementType,
    pub accrual_mode: LeaveAccrualMode,
    #[serde(serialize_with = "ser_opt_f64")]
    pub monthly_accrual_days: Option<f64>,
    pub created_at: JsDate,
    pub updated_at: Option<JsDate>,
}

/// Kolom `leavePolicy` (enum dibaca `::text`). Ganti `p.` bila memakai alias lain.
pub const POLICY_COLUMNS: &str = r#"p.id, p."companyId", p."annualLeaveDays", p."sickLeaveDays",
    p."sickLeaveMode"::text AS "sickLeaveMode", p."carryOverEnabled", p."maxCarryOverDays",
    p."carryOverExpiryMonth", p."resetType"::text AS "resetType",
    p."entitlementType"::text AS "entitlementType", p."accrualMode"::text AS "accrualMode",
    p."monthlyAccrualDays", p."createdAt", p."updatedAt""#;

/// Model `leaveBalance`.
#[derive(Debug, Clone, PartialEq, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
#[sqlx(rename_all = "camelCase")]
pub struct LeaveBalance {
    pub id: i32,
    pub user_id: i32,
    pub company_id: i32,
    pub year: i32,
    #[serde(serialize_with = "ser_f64")]
    pub annual_leave_total: f64,
    #[serde(serialize_with = "ser_f64")]
    pub annual_leave_used: f64,
    #[serde(serialize_with = "ser_f64")]
    pub sick_leave_total: f64,
    #[serde(serialize_with = "ser_f64")]
    pub sick_leave_used: f64,
    #[serde(serialize_with = "ser_f64")]
    pub carry_over_days: f64,
    #[serde(serialize_with = "ser_f64")]
    pub carry_over_used: f64,
    pub accrued_through_month: i32,
    pub created_at: JsDate,
    pub updated_at: Option<JsDate>,
}

/// Kolom `leaveBalance` dengan alias `b`.
pub const BALANCE_COLUMNS: &str = r#"b.id, b."userId", b."companyId", b.year, b."annualLeaveTotal",
    b."annualLeaveUsed", b."sickLeaveTotal", b."sickLeaveUsed", b."carryOverDays",
    b."carryOverUsed", b."accruedThroughMonth", b."createdAt", b."updatedAt""#;

/// Model `leaveTier`.
#[derive(Debug, Clone, PartialEq, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
#[sqlx(rename_all = "camelCase")]
pub struct LeaveTier {
    pub id: i32,
    pub company_id: i32,
    pub policy_id: i32,
    pub name: String,
    pub min_tenure_months: i32,
    pub max_tenure_months: Option<i32>,
    pub annual_leave_days: i32,
    pub created_at: JsDate,
    pub updated_at: Option<JsDate>,
}

/// Kolom `leaveTier` dengan alias `t`.
pub const TIER_COLUMNS: &str = r#"t.id, t."companyId", t."policyId", t.name, t."minTenureMonths",
    t."maxTenureMonths", t."annualLeaveDays", t."createdAt", t."updatedAt""#;

/// Model `holidayLeaveDeduction`.
#[derive(Debug, Clone, PartialEq, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
#[sqlx(rename_all = "camelCase")]
pub struct HolidayLeaveDeduction {
    pub id: i32,
    pub company_id: i32,
    pub holiday_id: Option<i32>,
    pub user_id: i32,
    pub balance_id: i32,
    pub holiday_name: String,
    pub holiday_date: chrono::NaiveDate,
    pub status: String,
    #[serde(serialize_with = "ser_f64")]
    pub intended_days: f64,
    #[serde(serialize_with = "ser_f64")]
    pub carry_over_deducted: f64,
    #[serde(serialize_with = "ser_f64")]
    pub annual_deducted: f64,
    pub applied_at: JsDate,
    pub reversed_at: Option<JsDate>,
    pub created_at: JsDate,
    pub updated_at: Option<JsDate>,
}

/// Kolom `holidayLeaveDeduction` dengan alias `d`.
pub const DEDUCTION_COLUMNS: &str = r#"d.id, d."companyId", d."holidayId", d."userId", d."balanceId",
    d."holidayName", d."holidayDate", d.status, d."intendedDays", d."carryOverDeducted",
    d."annualDeducted", d."appliedAt", d."reversedAt", d."createdAt", d."updatedAt""#;

/// Model `companyHoliday` (hanya kolom yang dibaca domain cuti).
#[derive(Debug, Clone, PartialEq, FromRow)]
#[sqlx(rename_all = "camelCase")]
pub struct HolidayState {
    pub id: i32,
    pub name: String,
    pub date: chrono::NaiveDate,
    pub is_active: bool,
    pub deducts_leave: bool,
}
