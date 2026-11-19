//! Fondasi bersama endpoint operator Timemoz (dashboard admin internal).
//!
//! Padanan Node:
//! - `JwtAdminAuthGuard` / strategy `jwt-admin` (`src/jwt/jwt-admin.strategy.ts`) → extractor [`Operator`].
//! - `OperatorRolesGuard` + `@OperatorRoles(...)` (`src/operator-auth/`) → [`Operator::require`].
//! - `OperatorAuditInterceptor` + `@OperatorAuditAction(...)` (`src/operator-audit/operator-audit.interceptor.ts`)
//!   → [`audit_after_success`] (dipanggil handler SESUDAH sukses).
//! - `recordOperatorAction(tx, entry)` (`src/operator-audit/record-operator-action.ts`) → [`record`]
//!   (dipanggil di dalam transaksi pemanggil).
//!
//! Route operator di Node ber-`@PermissionExempt('separate-auth')` → pasang sebagai
//! `RouteSpec::public()` (tanpa gerbang user) lalu pakai extractor [`Operator`] di handler.
//! Route pra-login (`@PermissionExempt('no-user')`) tidak memakai extractor.
//!
//! Extractor juga mengisi `store.operator` (`AuditHandle::set_operator`) seperti interceptor
//! class-level Node, supaya `auditLog` model ber-audit mencatat "Timemoz Support (<nama>)".

mod audit;
mod auth;

pub use audit::{
    AuditCtx, AuditMeta, CompanyFrom, OperatorAuditEntry, audit_after_success, record,
    strip_secrets,
};
pub use auth::{Operator, OperatorRole, decode_operator_claims};
