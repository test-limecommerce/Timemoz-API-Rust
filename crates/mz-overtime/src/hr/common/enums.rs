//! Port `src/overtime/enums/*`. Enum Postgres Prisma dibaca `col::text` dan ditulis
//! `$1::"OvertimeRequestStatus"` (lihat `mz_core::text_enum!`).
//!
//! `OvertimeDayType` = [`crate::calendar::DayType`] (nilai `WORKDAY`/`HOLIDAY` setara);
//! diekspor ulang di sini sebagai [`OvertimeDayType`].

pub use crate::calendar::DayType as OvertimeDayType;

mz_core::text_enum! {
    /// `OvertimeRequestStatus` (`overtime-request-status.enum.ts`).
    pub enum OvertimeRequestStatus {
        PENDING_APPROVAL,
        PENDING_EMPLOYEE,
        APPROVED,
        REJECTED,
        CANCELLED,
        COMPLETED,
    }
}

mz_core::text_enum! {
    /// `OvertimeRequestKind` (`overtime-request-kind.enum.ts`).
    pub enum OvertimeRequestKind { SELF, ORDER }
}

mz_core::text_enum! {
    /// `OvertimeCancelReason` (`overtime-cancel-reason.enum.ts`).
    pub enum OvertimeCancelReason {
        BY_REQUESTER,
        NO_RESPONSE,
        EMPLOYEE_ON_LEAVE,
        FEATURE_DISABLED,
    }
}

/// `CANCEL_REASON_LABEL`: teks alasan pembatalan yang ditampilkan ke pengguna.
pub const CANCEL_REASON_LABEL: &[(OvertimeCancelReason, &str)] = &[
    (OvertimeCancelReason::BY_REQUESTER, "Dibatalkan pengaju"),
    (
        OvertimeCancelReason::NO_RESPONSE,
        "Tidak direspons sampai batas waktu",
    ),
    (OvertimeCancelReason::EMPLOYEE_ON_LEAVE, "Karyawan cuti"),
    (
        OvertimeCancelReason::FEATURE_DISABLED,
        "Fitur lembur dinonaktifkan",
    ),
];

impl OvertimeCancelReason {
    /// Label tampilan (`CANCEL_REASON_LABEL[reason]`).
    pub fn label(&self) -> &'static str {
        CANCEL_REASON_LABEL
            .iter()
            .find(|(r, _)| r == self)
            .map_or("", |(_, l)| l)
    }
}

/// Teks daftar nilai untuk pesan `@IsEnum(OvertimeRequestStatus)` class-validator
/// (`PENDING_APPROVAL, PENDING_EMPLOYEE, …`).
pub fn status_enum_values() -> String {
    OvertimeRequestStatus::ALL
        .iter()
        .map(OvertimeRequestStatus::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn labels_match_node() {
        assert_eq!(
            OvertimeCancelReason::NO_RESPONSE.label(),
            "Tidak direspons sampai batas waktu"
        );
        for r in OvertimeCancelReason::ALL {
            assert!(!r.label().is_empty());
        }
        assert_eq!(
            status_enum_values(),
            "PENDING_APPROVAL, PENDING_EMPLOYEE, APPROVED, REJECTED, CANCELLED, COMPLETED"
        );
        assert_eq!(
            OvertimeDayType::parse("HOLIDAY"),
            Some(OvertimeDayType::Holiday)
        );
    }
}
