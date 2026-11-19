//! Port sisi TULIS audit log Mudah-API (`src/audit-logs/*` + `src/request-context/*`).
//!
//! Node menulis `auditLog` otomatis lewat middleware Prisma; sqlx tidak punya
//! middleware, jadi pemanggil memakai [`AuditHandle`] secara eksplisit (pra-baca →
//! tulis → `record_*`). Baris yang dihasilkan identik dengan Node untuk perubahan yang
//! sama. Lihat `README.md` untuk cara pakai dan `plan/deviations/mz-audit.md`.

mod admin;
mod build;
mod columns;
mod handle;
mod layer;
mod policy;
mod read;
mod row;
mod scope;
mod signature;
mod sink;

pub use admin::{
    AUDIT_PRUNE_BATCH_SIZE, AUDIT_RETENTION_DAYS, PruneResult, cron_jobs, labels, prune_expired,
    router,
};
pub use build::{AuditEntry, MemberEffect, PreRead};
pub use handle::{AuditHandle, DiscardGuard, MembershipPre, Touched, is_enabled};
pub use layer::{AuditLayer, AuditService, layer};
pub use policy::{diff_snapshots, is_excluded_field, is_secret_field, project_row, sanitize_where};
pub use read::{Columns, Lookup, read_rows};
pub use row::{Kind, Row};
pub use scope::{AUDIT_BULK_ENTITY_ID, AUDIT_BULK_THRESHOLD, Action, ActorType, EntityType, Model};
pub use signature::{detect_transition, has_required_fields};
pub use sink::WriteContext;
