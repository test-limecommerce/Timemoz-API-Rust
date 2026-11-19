//! Fondasi bersama route HR lembur. Modul `requests`, `approvals`, `reports` memakai API
//! di sini — JANGAN menyalinnya. Ringkasan signature: `plan/ot-hr-common-api.md`.
//!
//! | Submodul | Node |
//! |---|---|
//! | [`enums`] | `enums/*` |
//! | [`constants`] | `overtime.constants.ts` + bawaan `overtime-settings.service.ts` |
//! | [`errors`] | `overtime-errors.ts` |
//! | [`mapper`] | `overtime-request.mapper.ts` |
//! | [`eligibility`] | `overtime-eligibility.ts` |
//! | [`decidable`] | `overtime-decidable.ts` |
//! | [`mail`] | `overtime-mail.ts` + `MailService.sendOvertime*` |
//! | [`quota`] | `overtime-quota.service.ts` |
//! | [`clock`] | `companyTimeZoneOrUtc`, `companyClock`, `currentOffDays` |
//! | [`settings_svc`] | `overtime-settings.service.ts` |
//! | [`dto`] | `dto/date-key.validator.ts`, `reject-overtime.dto.ts`, `confirm-over-quota.dto.ts`, `overtime-request-status-filter.ts` |
//!
//! Lock (`lockEmployee`, `lockSettingsForShare/ForUpdate`, `lockWeek`) ada di
//! [`crate::locks`].

pub mod clock;
pub mod constants;
pub mod decidable;
pub mod dto;
pub mod eligibility;
pub mod enums;
pub mod errors;
pub mod mail;
pub mod mapper;
pub mod quota;
pub mod settings_svc;
