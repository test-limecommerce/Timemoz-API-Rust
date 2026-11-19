//! DTO modul konfigurasi cuti (`leave-types/dto/*`, `leave-policies/dto/*`, query kelas di
//! controller). Field berupa `Option<Value>` supaya "tidak dikirim" vs `null` vs tipe salah bisa
//! dibedakan persis seperti class-validator; konversi bertipe sesudah validasi lewat
//! `typed()`/`tri_*`.

use mz_core::ApiError;
use mz_core::extract::Validate;
use mz_core::jsfmt;
use serde::Deserialize;
use serde_json::Value;

use crate::validate::{
    Num, Tri, check_bool, check_enum, check_int, check_string, is_string, max_length_ok, node_500,
    present, tri_bool, tri_enum, tri_f64, tri_int, tri_string,
};
use mz_leave::enums::{
    LeaveAccrualMode, LeaveBalanceSource, LeaveEntitlementType, LeaveResetType, LeaveSickMode,
};

const BALANCE_SOURCES: &[&str] = &["ANNUAL", "SICK"];
const SICK_MODES: &[&str] = &["DEDUCT_ANNUAL", "UNLIMITED", "LIMITED"];
const RESET_TYPES: &[&str] = &["ANNUAL", "ANNIVERSARY"];
const ENTITLEMENT_TYPES: &[&str] = &["UNIFORM", "TIER_BASED"];
const ACCRUAL_MODES: &[&str] = &["UPFRONT", "MONTHLY"];
const BALANCE_FIELDS: &[&str] = &["annualLeaveTotal", "sickLeaveTotal", "carryOverDays"];

// ── leave-types ──────────────────────────────────────────────────────────────

/// Isi `CreateLeaveTypeDto` / `UpdateLeaveTypeDto` (`PartialType`: sama, semua opsional).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaveTypeBody {
    #[serde(default, deserialize_with = "present")]
    pub name: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub code: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub deduct_balance: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub balance_source: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub requires_document: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub annual_quota: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub is_active: Option<Value>,
}

/// `@Matches(/^[A-Z0-9_]+$/)`.
fn code_matches(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::String(s))
        if !s.is_empty() && s.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
}

impl LeaveTypeBody {
    /// `require_name`: `CreateLeaveTypeDto` (name wajib) vs `UpdateLeaveTypeDto`.
    fn check(&self, require_name: bool) -> Vec<String> {
        let mut e = Vec::new();
        check_string(&mut e, "name", self.name.as_ref(), !require_name, 100);
        // @IsString @MaxLength(50) @Matches @IsOptional → Matches, MaxLength, IsString.
        if !matches!(self.code, None | Some(Value::Null)) {
            let v = self.code.as_ref();
            if !code_matches(v) {
                e.push("code hanya boleh huruf kapital, angka, dan underscore".to_owned());
            }
            if !max_length_ok(v, 50) {
                e.push("code must be shorter than or equal to 50 characters".to_owned());
            }
            if !is_string(v) {
                e.push("code must be a string".to_owned());
            }
        }
        check_bool(&mut e, "deductBalance", self.deduct_balance.as_ref());
        check_enum(
            &mut e,
            "balanceSource",
            self.balance_source.as_ref(),
            BALANCE_SOURCES,
        );
        check_bool(&mut e, "requiresDocument", self.requires_document.as_ref());
        check_int(
            &mut e,
            "annualQuota",
            &Num::of(self.annual_quota.as_ref()),
            true,
            Some(1.0),
            None,
        );
        check_bool(&mut e, "isActive", self.is_active.as_ref());
        e
    }

    pub fn typed(&self) -> Result<LeaveTypeInput, ApiError> {
        Ok(LeaveTypeInput {
            name: tri_string(self.name.as_ref()),
            code: tri_string(self.code.as_ref()),
            deduct_balance: tri_bool(self.deduct_balance.as_ref()),
            balance_source: tri_enum(self.balance_source.as_ref()),
            requires_document: tri_bool(self.requires_document.as_ref()),
            annual_quota: tri_int(self.annual_quota.as_ref(), "annualQuota")?,
            is_active: tri_bool(self.is_active.as_ref()),
        })
    }
}

/// Hasil konversi bertipe `LeaveTypeBody`.
#[derive(Debug, Clone, PartialEq)]
pub struct LeaveTypeInput {
    pub name: Tri<String>,
    pub code: Tri<String>,
    pub deduct_balance: Tri<bool>,
    pub balance_source: Tri<LeaveBalanceSource>,
    pub requires_document: Tri<bool>,
    pub annual_quota: Tri<i32>,
    pub is_active: Tri<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct CreateLeaveTypeDto(pub LeaveTypeBody);

impl Validate for CreateLeaveTypeDto {
    fn validate(&self) -> Vec<String> {
        self.0.check(true)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct UpdateLeaveTypeDto(pub LeaveTypeBody);

impl Validate for UpdateLeaveTypeDto {
    fn validate(&self) -> Vec<String> {
        self.0.check(false)
    }
}

/// `LeaveTypeQueryDto`: `@IsBoolean @IsOptional @Type(() => Boolean)` — `Boolean("false")` adalah
/// `true`, hanya string kosong yang `false`; array tetap array (gagal `@IsBoolean`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaveTypeQuery {
    #[serde(default, deserialize_with = "present")]
    pub is_active: Option<Value>,
}

impl Validate for LeaveTypeQuery {
    fn validate(&self) -> Vec<String> {
        match &self.is_active {
            Some(Value::Array(_)) => vec!["isActive must be a boolean value".to_owned()],
            _ => Vec::new(),
        }
    }
}

impl LeaveTypeQuery {
    /// Filter `isActive` (`None` = tidak difilter).
    pub fn filter(&self) -> Option<bool> {
        match self.is_active.as_ref()? {
            Value::String(s) => Some(!s.is_empty()),
            Value::Null | Value::Array(_) => None,
            _ => Some(true),
        }
    }
}

// ── leave-policies ───────────────────────────────────────────────────────────

/// `UpdateLeavePolicyDto`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateLeavePolicyDto {
    #[serde(default, deserialize_with = "present")]
    pub annual_leave_days: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub sick_leave_days: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub sick_leave_mode: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub carry_over_enabled: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub max_carry_over_days: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub carry_over_expiry_month: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub reset_type: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub entitlement_type: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub accrual_mode: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub monthly_accrual_days: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub approver_ids: Option<Value>,
}

/// `IsHalfStep`: `typeof value === 'number' && Number.isInteger(value * 2)`.
fn is_half_step(n: &Num) -> bool {
    matches!(n, Num::Val(f) if (f * 2.0).is_finite() && (f * 2.0).fract() == 0.0)
}

/// `@IsNumber({ maxDecimalPlaces: 1 })` (tanpa NaN/Infinity).
fn is_number_one_decimal(n: &Num) -> bool {
    match n {
        Num::Val(f) if f.is_finite() => {
            if f.fract() == 0.0 {
                return true;
            }
            let text = jsfmt::num_string(*f);
            text.split_once('.').is_none_or(|(_, d)| d.len() <= 1)
        }
        _ => false,
    }
}

impl Validate for UpdateLeavePolicyDto {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        let num = |v: &Option<Value>| Num::of(v.as_ref());
        check_int(
            &mut e,
            "annualLeaveDays",
            &num(&self.annual_leave_days),
            true,
            Some(0.0),
            None,
        );
        check_int(
            &mut e,
            "sickLeaveDays",
            &num(&self.sick_leave_days),
            true,
            Some(0.0),
            None,
        );
        check_enum(
            &mut e,
            "sickLeaveMode",
            self.sick_leave_mode.as_ref(),
            SICK_MODES,
        );
        check_bool(&mut e, "carryOverEnabled", self.carry_over_enabled.as_ref());
        check_int(
            &mut e,
            "maxCarryOverDays",
            &num(&self.max_carry_over_days),
            true,
            Some(0.0),
            None,
        );
        check_int(
            &mut e,
            "carryOverExpiryMonth",
            &num(&self.carry_over_expiry_month),
            true,
            Some(1.0),
            Some(12.0),
        );
        check_enum(&mut e, "resetType", self.reset_type.as_ref(), RESET_TYPES);
        check_enum(
            &mut e,
            "entitlementType",
            self.entitlement_type.as_ref(),
            ENTITLEMENT_TYPES,
        );
        check_enum(
            &mut e,
            "accrualMode",
            self.accrual_mode.as_ref(),
            ACCRUAL_MODES,
        );

        // @IsNumber({maxDecimalPlaces:1}) @Min(0.5) @Max(31) @Validate(IsHalfStep) @IsOptional:
        // IsHalfStep, Max, Min, IsNumber.
        let monthly = num(&self.monthly_accrual_days);
        if !monthly.skipped() {
            if !is_half_step(&monthly) {
                e.push("monthlyAccrualDays harus kelipatan 0,5".to_owned());
            }
            if !monthly.at_most(31.0) {
                e.push("monthlyAccrualDays must not be greater than 31".to_owned());
            }
            if !monthly.at_least(0.5) {
                e.push("monthlyAccrualDays must not be less than 0.5".to_owned());
            }
            if !is_number_one_decimal(&monthly) {
                e.push(
                    "monthlyAccrualDays must be a number conforming to the specified constraints"
                        .to_owned(),
                );
            }
        }

        // @IsArray @IsInt({each}) @IsPositive({each}) @IsOptional @Type(() => Number):
        // IsPositive each, IsInt each, IsArray. `each` pada non-array memeriksa nilainya sendiri.
        let ids = num(&self.approver_ids);
        if !ids.skipped() {
            let items: Vec<&Num> = match &ids {
                Num::Arr(items) => items.iter().collect(),
                other => vec![other],
            };
            if !items.iter().all(|n| n.at_least(f64::MIN_POSITIVE)) {
                e.push("each value in approverIds must be a positive number".to_owned());
            }
            if !items.iter().all(|n| n.is_int()) {
                e.push("each value in approverIds must be an integer number".to_owned());
            }
            if !matches!(ids, Num::Arr(_)) {
                e.push("approverIds must be an array".to_owned());
            }
        }
        e
    }
}

/// Hasil konversi bertipe `UpdateLeavePolicyDto`.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyInput {
    pub annual_leave_days: Tri<i32>,
    pub sick_leave_days: Tri<i32>,
    pub sick_leave_mode: Tri<LeaveSickMode>,
    pub carry_over_enabled: Tri<bool>,
    pub max_carry_over_days: Tri<i32>,
    pub carry_over_expiry_month: Tri<i32>,
    pub reset_type: Tri<LeaveResetType>,
    pub entitlement_type: Tri<LeaveEntitlementType>,
    pub accrual_mode: Tri<LeaveAccrualMode>,
    pub monthly_accrual_days: Tri<f64>,
    /// `[...new Set(dto.approverIds)]` (urutan kemunculan pertama); `null` → `new Set(null)` = kosong.
    pub approver_ids: Option<Vec<i32>>,
}

impl UpdateLeavePolicyDto {
    pub fn typed(&self) -> Result<PolicyInput, ApiError> {
        let approver_ids = match Num::of(self.approver_ids.as_ref()) {
            Num::Absent => None,
            Num::Null => Some(Vec::new()),
            Num::Val(f) => Some(vec![
                jsfmt::as_i32(f).ok_or_else(|| node_500("approverIds"))?,
            ]),
            Num::Arr(items) => {
                let mut out: Vec<i32> = Vec::new();
                for item in items {
                    let id = item
                        .as_f64()
                        .and_then(jsfmt::as_i32)
                        .ok_or_else(|| node_500("approverIds"))?;
                    if !out.contains(&id) {
                        out.push(id);
                    }
                }
                Some(out)
            }
        };
        Ok(PolicyInput {
            annual_leave_days: tri_int(self.annual_leave_days.as_ref(), "annualLeaveDays")?,
            sick_leave_days: tri_int(self.sick_leave_days.as_ref(), "sickLeaveDays")?,
            sick_leave_mode: tri_enum(self.sick_leave_mode.as_ref()),
            carry_over_enabled: tri_bool(self.carry_over_enabled.as_ref()),
            max_carry_over_days: tri_int(self.max_carry_over_days.as_ref(), "maxCarryOverDays")?,
            carry_over_expiry_month: tri_int(
                self.carry_over_expiry_month.as_ref(),
                "carryOverExpiryMonth",
            )?,
            reset_type: tri_enum(self.reset_type.as_ref()),
            entitlement_type: tri_enum(self.entitlement_type.as_ref()),
            accrual_mode: tri_enum(self.accrual_mode.as_ref()),
            monthly_accrual_days: tri_f64(
                self.monthly_accrual_days.as_ref(),
                "monthlyAccrualDays",
            )?,
            approver_ids,
        })
    }
}

/// `UpdateEmployeeLeavePolicyDto`: `@IsInt @Min(0) @Type(() => Number) annualLeaveDays` (wajib).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateEmployeeLeavePolicyDto {
    #[serde(default, deserialize_with = "present")]
    pub annual_leave_days: Option<Value>,
}

impl Validate for UpdateEmployeeLeavePolicyDto {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        check_int(
            &mut e,
            "annualLeaveDays",
            &Num::of(self.annual_leave_days.as_ref()),
            false,
            Some(0.0),
            None,
        );
        e
    }
}

impl UpdateEmployeeLeavePolicyDto {
    pub fn annual_leave_days(&self) -> Result<i32, ApiError> {
        match tri_int(self.annual_leave_days.as_ref(), "annualLeaveDays")? {
            Tri::Val(v) => Ok(v),
            _ => Err(node_500("annualLeaveDays")),
        }
    }
}

/// `CreateLeaveTierDto` (dipakai juga oleh `PUT tiers/:tierId`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateLeaveTierDto {
    #[serde(default, deserialize_with = "present")]
    pub name: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub min_tenure_months: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub max_tenure_months: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub annual_leave_days: Option<Value>,
}

impl Validate for CreateLeaveTierDto {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        check_string(&mut e, "name", self.name.as_ref(), false, 100);
        check_int(
            &mut e,
            "minTenureMonths",
            &Num::of(self.min_tenure_months.as_ref()),
            true,
            Some(0.0),
            None,
        );
        check_int(
            &mut e,
            "maxTenureMonths",
            &Num::of(self.max_tenure_months.as_ref()),
            true,
            Some(0.0),
            None,
        );
        check_int(
            &mut e,
            "annualLeaveDays",
            &Num::of(self.annual_leave_days.as_ref()),
            false,
            Some(0.0),
            None,
        );
        e
    }
}

/// Hasil konversi bertipe `CreateLeaveTierDto`.
#[derive(Debug, Clone, PartialEq)]
pub struct TierInput {
    pub name: String,
    pub min_tenure_months: Tri<i32>,
    pub max_tenure_months: Tri<i32>,
    pub annual_leave_days: i32,
}

impl CreateLeaveTierDto {
    pub fn typed(&self) -> Result<TierInput, ApiError> {
        let name = match tri_string(self.name.as_ref()) {
            Tri::Val(s) => s,
            _ => return Err(node_500("name")),
        };
        let annual_leave_days = match tri_int(self.annual_leave_days.as_ref(), "annualLeaveDays")? {
            Tri::Val(v) => v,
            _ => return Err(node_500("annualLeaveDays")),
        };
        Ok(TierInput {
            name,
            min_tenure_months: tri_int(self.min_tenure_months.as_ref(), "minTenureMonths")?,
            max_tenure_months: tri_int(self.max_tenure_months.as_ref(), "maxTenureMonths")?,
            annual_leave_days,
        })
    }
}

/// `AdjustLeaveBalanceDto`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdjustLeaveBalanceDto {
    #[serde(default, deserialize_with = "present")]
    pub year: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub field: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub delta: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub reason: Option<Value>,
}

impl Validate for AdjustLeaveBalanceDto {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        check_int(
            &mut e,
            "year",
            &Num::of(self.year.as_ref()),
            true,
            Some(2020.0),
            None,
        );
        // @IsEnum (wajib: tanpa @IsOptional).
        match self.field.as_ref() {
            Some(Value::String(s)) if BALANCE_FIELDS.contains(&s.as_str()) => {}
            _ => e.push(format!(
                "field must be one of the following values: {}",
                BALANCE_FIELDS.join(", ")
            )),
        }
        // @IsNumber @NotEquals(0, 'delta tidak boleh 0') @Type(() => Number): NotEquals, IsNumber.
        let delta = Num::of(self.delta.as_ref());
        if matches!(delta, Num::Val(f) if f == 0.0) {
            e.push("delta tidak boleh 0".to_owned());
        }
        if !matches!(delta, Num::Val(f) if f.is_finite()) {
            e.push("delta must be a number conforming to the specified constraints".to_owned());
        }
        // @IsString @IsNotEmpty('reason wajib diisi') @MaxLength(500): MaxLength, IsNotEmpty, IsString.
        let reason = self.reason.as_ref();
        if !max_length_ok(reason, 500) {
            e.push("reason must be shorter than or equal to 500 characters".to_owned());
        }
        if matches!(reason, None | Some(Value::Null))
            || matches!(reason, Some(Value::String(s)) if s.is_empty())
        {
            e.push("reason wajib diisi".to_owned());
        }
        if !is_string(reason) {
            e.push("reason must be a string".to_owned());
        }
        e
    }
}

/// Hasil konversi bertipe `AdjustLeaveBalanceDto`.
#[derive(Debug, Clone, PartialEq)]
pub struct AdjustInput {
    pub year: Option<i32>,
    pub field: &'static str,
    pub delta: f64,
    pub reason: String,
}

impl AdjustLeaveBalanceDto {
    pub fn typed(&self) -> Result<AdjustInput, ApiError> {
        let year = match tri_int(self.year.as_ref(), "year")? {
            Tri::Val(y) => Some(y),
            _ => None,
        };
        let field = match self.field.as_ref().and_then(Value::as_str) {
            Some("annualLeaveTotal") => "annualLeaveTotal",
            Some("sickLeaveTotal") => "sickLeaveTotal",
            Some("carryOverDays") => "carryOverDays",
            _ => return Err(node_500("field")),
        };
        let delta = match tri_f64(self.delta.as_ref(), "delta")? {
            Tri::Val(d) => d,
            _ => return Err(node_500("delta")),
        };
        let reason = match tri_string(self.reason.as_ref()) {
            Tri::Val(r) => r,
            _ => return Err(node_500("reason")),
        };
        Ok(AdjustInput {
            year,
            field,
            delta,
            reason,
        })
    }
}

/// `YearQueryDto`: `@IsInt @IsOptional @Type(() => Number) year`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YearQuery {
    #[serde(default, deserialize_with = "present")]
    pub year: Option<Value>,
}

impl Validate for YearQuery {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        check_int(
            &mut e,
            "year",
            &Num::of(self.year.as_ref()),
            true,
            None,
            None,
        );
        e
    }
}

impl YearQuery {
    pub fn year(&self) -> Result<Option<i32>, ApiError> {
        Ok(match tri_int(self.year.as_ref(), "year")? {
            Tri::Val(y) => Some(y),
            _ => None,
        })
    }
}

/// `BalancesQueryDto extends YearQueryDto`: properti sendiri dulu (page, limit, search), lalu
/// `year` dari kelas induk (class-validator menaruh metadata turunan di depan).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BalancesQuery {
    #[serde(default, deserialize_with = "present")]
    pub page: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub limit: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub search: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub year: Option<Value>,
}

impl Validate for BalancesQuery {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        check_int(
            &mut e,
            "page",
            &Num::of(self.page.as_ref()),
            true,
            Some(1.0),
            None,
        );
        check_int(
            &mut e,
            "limit",
            &Num::of(self.limit.as_ref()),
            true,
            Some(1.0),
            None,
        );
        if !matches!(
            self.search,
            None | Some(Value::Null) | Some(Value::String(_))
        ) {
            e.push("search must be a string".to_owned());
        }
        check_int(
            &mut e,
            "year",
            &Num::of(self.year.as_ref()),
            true,
            None,
            None,
        );
        e
    }
}

/// Query `balances` bertipe.
#[derive(Debug, Clone, PartialEq)]
pub struct BalancesParams {
    pub year: Option<i32>,
    pub page: Option<i32>,
    pub limit: Option<i32>,
    pub search: Option<String>,
}

impl BalancesQuery {
    pub fn typed(&self) -> Result<BalancesParams, ApiError> {
        let int = |v: &Option<Value>, f: &str| -> Result<Option<i32>, ApiError> {
            Ok(match tri_int(v.as_ref(), f)? {
                Tri::Val(n) => Some(n),
                _ => None,
            })
        };
        Ok(BalancesParams {
            year: int(&self.year, "year")?,
            page: int(&self.page, "page")?,
            limit: int(&self.limit, "limit")?,
            search: match tri_string(self.search.as_ref()) {
                Tri::Val(s) => Some(s),
                _ => None,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy(v: Value) -> UpdateLeavePolicyDto {
        serde_json::from_value(v).unwrap_or_default()
    }

    #[test]
    fn leave_type_messages() {
        let dto: CreateLeaveTypeDto =
            serde_json::from_value(json!({"code": "a-b", "annualQuota": 0})).unwrap_or_default();
        assert_eq!(
            dto.validate(),
            [
                "name must be shorter than or equal to 100 characters",
                "name must be a string",
                "code hanya boleh huruf kapital, angka, dan underscore",
                "annualQuota must not be less than 1",
            ]
        );
        let ok: UpdateLeaveTypeDto =
            serde_json::from_value(json!({"annualQuota": null, "isActive": false}))
                .unwrap_or_default();
        assert!(ok.validate().is_empty());
        let long = "A".repeat(51);
        let dto: UpdateLeaveTypeDto =
            serde_json::from_value(json!({ "code": long })).unwrap_or_default();
        assert_eq!(
            dto.validate(),
            ["code must be shorter than or equal to 50 characters"]
        );
    }

    #[test]
    fn policy_monthly_messages() {
        let d = policy(json!({"monthlyAccrualDays": 0.25}));
        assert_eq!(
            d.validate(),
            [
                "monthlyAccrualDays harus kelipatan 0,5",
                "monthlyAccrualDays must not be less than 0.5",
                "monthlyAccrualDays must be a number conforming to the specified constraints",
            ]
        );
        assert!(
            policy(json!({"monthlyAccrualDays": 1.5}))
                .validate()
                .is_empty()
        );
        assert!(
            policy(json!({"monthlyAccrualDays": "2"}))
                .validate()
                .is_empty()
        );
        assert!(
            policy(json!({"monthlyAccrualDays": null}))
                .validate()
                .is_empty()
        );
        let d = policy(json!({"monthlyAccrualDays": 32}));
        assert_eq!(
            d.validate(),
            ["monthlyAccrualDays must not be greater than 31"]
        );
    }

    #[test]
    fn policy_approver_messages() {
        assert!(
            policy(json!({"approverIds": [1, "2"]}))
                .validate()
                .is_empty()
        );
        assert_eq!(
            policy(json!({"approverIds": [1, -2, 1.5]})).validate(),
            [
                "each value in approverIds must be a positive number",
                "each value in approverIds must be an integer number"
            ]
        );
        assert_eq!(
            policy(json!({"approverIds": 5})).validate(),
            ["approverIds must be an array"]
        );
        assert_eq!(
            policy(json!({"approverIds": "x"})).validate(),
            [
                "each value in approverIds must be a positive number",
                "each value in approverIds must be an integer number",
                "approverIds must be an array"
            ]
        );
        let t = policy(json!({"approverIds": [3, "3", 4]}))
            .typed()
            .map(|p| p.approver_ids);
        assert_eq!(t.ok(), Some(Some(vec![3, 4])));
        let t = policy(json!({"approverIds": null}))
            .typed()
            .map(|p| p.approver_ids);
        assert_eq!(t.ok(), Some(Some(Vec::new())));
    }

    #[test]
    fn policy_expiry_and_enum_messages() {
        let d = policy(json!({
            "carryOverExpiryMonth": 13, "sickLeaveMode": "NOPE", "annualLeaveDays": -1
        }));
        assert_eq!(
            d.validate(),
            [
                "annualLeaveDays must not be less than 0",
                "sickLeaveMode must be one of the following values: DEDUCT_ANNUAL, UNLIMITED, LIMITED",
                "carryOverExpiryMonth must not be greater than 12",
            ]
        );
    }

    #[test]
    fn adjust_messages() {
        let d: AdjustLeaveBalanceDto = serde_json::from_value(json!({
            "year": 2019, "field": "x", "delta": 0, "reason": ""
        }))
        .unwrap_or_default();
        assert_eq!(
            d.validate(),
            [
                "year must not be less than 2020",
                "field must be one of the following values: annualLeaveTotal, sickLeaveTotal, carryOverDays",
                "delta tidak boleh 0",
                "reason wajib diisi",
            ]
        );
        let d: AdjustLeaveBalanceDto = serde_json::from_value(json!({})).unwrap_or_default();
        assert_eq!(
            d.validate(),
            [
                "field must be one of the following values: annualLeaveTotal, sickLeaveTotal, carryOverDays",
                "delta must be a number conforming to the specified constraints",
                "reason must be shorter than or equal to 500 characters",
                "reason wajib diisi",
                "reason must be a string",
            ]
        );
        let ok: AdjustLeaveBalanceDto = serde_json::from_value(json!({
            "field": "carryOverDays", "delta": "-1.5", "reason": " "
        }))
        .unwrap_or_default();
        assert!(ok.validate().is_empty());
        assert_eq!(ok.typed().map(|t| t.delta).ok(), Some(-1.5));
    }

    #[test]
    fn tier_and_employee_policy_messages() {
        let d: CreateLeaveTierDto =
            serde_json::from_value(json!({"minTenureMonths": -1})).unwrap_or_default();
        assert_eq!(
            d.validate(),
            [
                "name must be shorter than or equal to 100 characters",
                "name must be a string",
                "minTenureMonths must not be less than 0",
                "annualLeaveDays must not be less than 0",
                "annualLeaveDays must be an integer number",
            ]
        );
        let d: UpdateEmployeeLeavePolicyDto =
            serde_json::from_value(json!({"annualLeaveDays": null})).unwrap_or_default();
        assert_eq!(d.validate().len(), 2);
    }

    #[test]
    fn query_messages() {
        let q: BalancesQuery =
            serde_json::from_value(json!({"page": "0", "limit": "x", "year": "abc"}))
                .unwrap_or_default();
        assert_eq!(
            q.validate(),
            [
                "page must not be less than 1",
                "limit must not be less than 1",
                "limit must be an integer number",
                "year must be an integer number",
            ]
        );
        let q: LeaveTypeQuery =
            serde_json::from_value(json!({"isActive": "false"})).unwrap_or_default();
        assert_eq!(q.filter(), Some(true));
        let q: LeaveTypeQuery = serde_json::from_value(json!({"isActive": ""})).unwrap_or_default();
        assert_eq!(q.filter(), Some(false));
    }
}
