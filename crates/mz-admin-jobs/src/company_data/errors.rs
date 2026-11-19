//! Port `src/admin-jobs/company-data/errors.ts`: error bertipe inti backup/restore/merge.
//!
//! Teks `message` sama persis dengan Node (dicatat di log job dan `adminJob.result.error`).

use std::fmt;

/// `CompanyDataErrorCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    InvalidParams,
    ScopeConfig,
    CompanyNotFound,
    CrossCompanyLeak,
    ManifestInvalid,
    SchemaMismatch,
    BackupFileMissing,
    ChecksumMismatch,
    MissingExternal,
    UniqueConflict,
    PartialUniqueConflict,
    IdAllocationFailed,
    PgConfig,
    LegacySource,
    MergeTargetMissing,
    MergeTargetMismatch,
    RetentionLimited,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidParams => "INVALID_PARAMS",
            Self::ScopeConfig => "SCOPE_CONFIG",
            Self::CompanyNotFound => "COMPANY_NOT_FOUND",
            Self::CrossCompanyLeak => "CROSS_COMPANY_LEAK",
            Self::ManifestInvalid => "MANIFEST_INVALID",
            Self::SchemaMismatch => "SCHEMA_MISMATCH",
            Self::BackupFileMissing => "BACKUP_FILE_MISSING",
            Self::ChecksumMismatch => "CHECKSUM_MISMATCH",
            Self::MissingExternal => "MISSING_EXTERNAL",
            Self::UniqueConflict => "UNIQUE_CONFLICT",
            Self::PartialUniqueConflict => "PARTIAL_UNIQUE_CONFLICT",
            Self::IdAllocationFailed => "ID_ALLOCATION_FAILED",
            Self::PgConfig => "PG_CONFIG",
            Self::LegacySource => "LEGACY_SOURCE",
            Self::MergeTargetMissing => "MERGE_TARGET_MISSING",
            Self::MergeTargetMismatch => "MERGE_TARGET_MISMATCH",
            Self::RetentionLimited => "RETENTION_LIMITED",
        }
    }
}

/// `CompanyDataError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompanyDataError {
    pub code: Code,
    pub message: String,
}

impl CompanyDataError {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for CompanyDataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CompanyDataError {}

/// Error inti: [`CompanyDataError`] bertipe, atau galat lain (SQL, I/O) yang di Node
/// dilempar apa adanya.
pub type CdResult<T> = anyhow::Result<T>;

/// `throw new CompanyDataError(code, message)`.
pub fn fail<T>(code: Code, message: impl Into<String>) -> CdResult<T> {
    Err(CompanyDataError::new(code, message).into())
}

/// Kode error bila `err` adalah [`CompanyDataError`].
pub fn code_of(err: &anyhow::Error) -> Option<Code> {
    err.downcast_ref::<CompanyDataError>().map(|e| e.code)
}

/// Penerima log baris demi baris (`CompanyDataLog`; level `warn` tidak dibedakan di worker,
/// sama seperti processor Node yang hanya meneruskan teksnya).
pub trait Log: Send + Sync {
    fn line(&self, line: &str);
}

impl<F: Fn(&str) + Send + Sync> Log for F {
    fn line(&self, line: &str) {
        self(line);
    }
}
