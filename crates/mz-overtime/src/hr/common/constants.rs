//! Port `src/overtime/overtime.constants.ts` + konstanta bawaan dari
//! `overtime-settings.service.ts`.

/// FR-033 — catatan hukum yang wajib tampil; sistem hanya memberi peringatan.
pub const LEGAL_NOTE: &str = "Batas lembur PP 35/2021: 4 jam/hari dan 18 jam/minggu di hari kerja; 12 jam (pola 5 hari) atau 11 jam (pola 6 hari) di hari libur. Sistem hanya memberi peringatan; kepatuhan menjadi tanggung jawab perusahaan.";

/// Label pelampauan mingguan (laporan).
pub const WEEKLY_EXCESS_LABEL: &str = "Melebihi jam kerja mingguan";

/// Baris setelan kosong = bawaan (FR-001, FR-007).
pub const DEFAULT_OVERTIME_ENABLED: bool = true;
/// `DEFAULT_RETRO_WINDOW_DAYS`.
pub const DEFAULT_RETRO_WINDOW_DAYS: i32 = 7;
/// Batas `retroWindowDays` (inklusif).
pub const MIN_RETRO_WINDOW_DAYS: i32 = 0;
pub const MAX_RETRO_WINDOW_DAYS: i32 = 31;
/// Batas daftar kandidat approver (dropdown dashboard).
pub const APPROVER_CANDIDATE_LIMIT: i64 = 200;

/// `users.status`, `userCompany.status`, `userAccessLevel.status`, `accessLevel.status`: 1 = aktif.
pub const ACTIVE: i32 = 1;
