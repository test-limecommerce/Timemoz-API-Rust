//! Port `src/store-visits/enums/*.ts`. Enum Prisma (`StoreVisitStatus`, `StoreVisitState`,
//! `StoreVisitMethod`, `GpsValidation`, `StoreVisitPhotoKind`, `StoreVisitAuditEvent`) dibaca
//! `col::text` dan ditulis `$n::"EnumName"`.

mz_core::text_enum! {
    /// `AdminStoreSortField`.
    pub enum AdminStoreSortField { name, code, createdAt, lastVisitAt }
}

mz_core::text_enum! {
    /// `ExportFormat` (§7.6): csv di-stream sampai 50.000 baris; xlsx dibangun di memori, plafon 5.000.
    pub enum ExportFormat { csv, xlsx }
}

mz_core::text_enum! {
    /// `GpsValidation` (enum Prisma).
    pub enum GpsValidation { valid, outside, mock, lowAccuracy }
}

mz_core::text_enum! {
    /// `MobileStoreSortField`.
    pub enum MobileStoreSortField { name, code, lastVisit, distance }
}

mz_core::text_enum! {
    /// `SortOrder`.
    pub enum SortOrder { asc, desc }
}

mz_core::text_enum! {
    /// `StoreGroupSortField`.
    pub enum StoreGroupSortField { name, code, createdAt }
}

mz_core::text_enum! {
    /// `StoreVisitAuditEvent` (enum Prisma).
    pub enum StoreVisitAuditEvent { mockLocationBlocked, outOfRadius, overrideUsed, outOfScope }
}

mz_core::text_enum! {
    /// `StoreVisitDateRange` (§7.4) — batas dihitung di `company.timeZone`.
    pub enum StoreVisitDateRange { today, yesterday, thisWeek, last7Days, thisMonth, lastMonth }
}

mz_core::text_enum! {
    /// `StoreVisitMethod` (enum Prisma). `qr`/`manual` = fase 2, ditolak API.
    pub enum StoreVisitMethod { gps, qr, manual }
}

mz_core::text_enum! {
    /// `StoreVisitPhotoKind` (enum Prisma).
    pub enum StoreVisitPhotoKind { store, selfie, result }
}

mz_core::text_enum! {
    /// `StoreVisitPlanComplianceGroupBy` (§4.3), bawaan `sales`.
    pub enum StoreVisitPlanComplianceGroupBy { sales, region }
}

mz_core::text_enum! {
    /// `StoreVisitPlanStatus` (§2.1) — seluruhnya turunan, tidak ada kolom.
    pub enum StoreVisitPlanStatus { pending, fulfilled, missed }
}

mz_core::text_enum! {
    /// `StoreVisitState` (enum Prisma).
    pub enum StoreVisitState { active, completed, cancelled }
}

mz_core::text_enum! {
    /// `StoreVisitStatus` (enum Prisma).
    pub enum StoreVisitStatus { success, closed, moved, noOwner, rejected, other }
}

/// `PhotoContentType` — hanya dua content type foto kunjungan (§6.5).
pub const PHOTO_CONTENT_TYPES: [&str; 2] = ["image/jpeg", "image/png"];

/// Daftar nilai enum untuk pesan `@IsEnum` (`"x must be one of the following values: a, b"`).
pub fn values_list(all: &[&str]) -> String {
    all.join(", ")
}

/// Pesan bawaan `@IsEnum(E)`.
pub fn is_enum_message(field: &str, values: &[&str]) -> String {
    format!(
        "{field} must be one of the following values: {}",
        values_list(values)
    )
}

/// `E::ALL` → `&[&str]` (untuk [`is_enum_message`]).
#[macro_export]
macro_rules! enum_values {
    ($e:ty) => {
        <$e>::ALL
            .iter()
            .map(|v| v.as_str())
            .collect::<Vec<&'static str>>()
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_strings() {
        assert_eq!(GpsValidation::lowAccuracy.as_str(), "lowAccuracy");
        assert_eq!(
            "noOwner".parse::<StoreVisitStatus>(),
            Ok(StoreVisitStatus::noOwner)
        );
        assert_eq!(
            is_enum_message("format", &enum_values!(ExportFormat)),
            "format must be one of the following values: csv, xlsx"
        );
    }
}
