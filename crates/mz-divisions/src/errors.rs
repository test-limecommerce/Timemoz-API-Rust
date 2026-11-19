//! Galat modul divisi = [`ApiError`] mz-core. Body `{ code, message, details? }` memakai
//! urutan kunci Node (`details` TERAKHIR): `json!`/struct diserialisasi dalam urutan penyisipan
//! (workspace memakai `serde_json` dengan fitur `preserve_order`).

use axum::http::StatusCode;
use mz_core::{ApiError, ApiResult};
use serde::Serialize;

use crate::consts::*;

pub type DivError = ApiError;
pub type DivResult<T> = ApiResult<T>;

#[derive(Serialize)]
struct WithDetails<'a, D: Serialize> {
    code: &'a str,
    message: &'a str,
    details: D,
}

/// `{ code, message }`.
pub fn coded(status: StatusCode, code: &str, message: impl Into<String>) -> DivError {
    ApiError::coded(status, code, message)
}

/// `{ code, message, details }`.
pub fn coded_details(
    status: StatusCode,
    code: &str,
    message: &str,
    details: impl Serialize,
) -> DivError {
    ApiError::serialized(
        status,
        &WithDetails {
            code,
            message,
            details,
        },
    )
}

/// `new ForbiddenException({ code: DIVISION_NOT_LEADER, ... })`.
pub fn not_leader() -> DivError {
    coded(
        StatusCode::FORBIDDEN,
        DIVISION_NOT_LEADER,
        "Only admins, managers and division Leaders can view divisions",
    )
}

/// `new DivisionScopeForbiddenException(message)`.
pub fn scope_forbidden(message: &str) -> DivError {
    coded(StatusCode::FORBIDDEN, DIVISION_SCOPE_FORBIDDEN, message)
}

pub fn division_not_found() -> DivError {
    coded(
        StatusCode::NOT_FOUND,
        DIVISION_NOT_FOUND,
        "Division not found",
    )
}

pub fn member_not_found() -> DivError {
    coded(
        StatusCode::NOT_FOUND,
        DIVISION_MEMBER_NOT_FOUND,
        "This employee is not a member of the division",
    )
}

pub fn stale() -> DivError {
    coded(
        StatusCode::CONFLICT,
        DIVISION_STALE,
        "This division was changed by someone else; reload and try again",
    )
}

pub fn leader_required(message: &str) -> DivError {
    coded(
        StatusCode::UNPROCESSABLE_ENTITY,
        DIVISION_LEADER_REQUIRED,
        message,
    )
}

pub fn name_taken() -> DivError {
    coded(
        StatusCode::CONFLICT,
        DIVISION_NAME_TAKEN,
        "A division with this name already exists",
    )
}

/// `new ForbiddenException({ code: MODULE_DISABLED, ... })` (`assertOpen`).
pub fn module_disabled() -> DivError {
    coded(
        StatusCode::FORBIDDEN,
        MODULE_DISABLED,
        "Divisions is not enabled for this company",
    )
}

/// Galat 500 (mis. `TypeError` Node pada body `null` yang lolos validasi).
pub fn internal(msg: &str) -> DivError {
    ApiError::internal(anyhow::anyhow!(msg.to_owned()))
}

/// `mapUniqueViolation`: pelanggaran unique yang lolos dari pemeriksaan di bawah lock.
///   - `division_company_name_unique_active` → 409 DIVISION_NAME_TAKEN;
///   - tabel `divisionMember` (index `(divisionId, userId)` aktif, atau index v1) → 409 DIVISION_STALE;
///   - selain itu `None` (pemanggil meneruskan galat aslinya).
pub fn map_unique_violation(e: &sqlx::Error) -> Option<DivError> {
    let sqlx::Error::Database(db) = e else {
        return None;
    };
    if !db.is_unique_violation() {
        return None;
    }
    let constraint = db.constraint().unwrap_or_default().to_lowercase();
    let table = db.table().unwrap_or_default();
    if table == "division" {
        return constraint.contains("name").then(name_taken);
    }
    // Kolom target Node: `userId` / `divisionId` (index aktif v1 dan A1); `uuid` bukan.
    if table == "divisionMember" && !constraint.ends_with("uuid_key") {
        return Some(coded(
            StatusCode::CONFLICT,
            DIVISION_STALE,
            "Division data changed at the same time; reload and try again",
        ));
    }
    None
}
