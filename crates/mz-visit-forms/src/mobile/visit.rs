//! Pembacaan tabel `storeVisit` / `store` untuk jalur mobile form — TITIK TEMU tahap S3.
//!
//! Modul store-visits belum di-port, jadi kunjungan dibaca lewat SQL eksplisit di sini
//! (sama dengan Node: `StoreVisitFormsModule` membaca `storeVisit` lewat query, bukan lewat
//! service modul kunjungan). Begitu `mz-store-visits` ada, fungsi di modul ini adalah satu-
//! satunya tempat yang perlu dialihkan. Node: `mobile-store-visit-forms.service.ts:33-165,520`.

use axum::http::StatusCode;
use chrono::NaiveDateTime;
use mz_core::{ApiError, ApiResult};
use sqlx::PgExecutor;

use crate::errors::{coded, not_found};

/// `ACTIVE_VISIT_STATE` — hanya kunjungan `active` yang boleh ditulis (§6.1).
pub const ACTIVE_VISIT_STATE: &str = "active";

/// `VISIT_SELECT` / `VisitContext` — kolom kunjungan yang dipakai modul ini, tidak lebih.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct VisitContext {
    pub id: i32,
    pub uuid: String,
    pub company_id: i32,
    pub user_id: i32,
    pub store_id: i32,
    /// `StoreVisitState` (`active` | `completed` | `cancelled`).
    pub state: String,
    pub check_in_at: NaiveDateTime,
}

const VISIT_COLUMNS: &str = r#"id, uuid::text AS uuid, "companyId" AS company_id, "userId" AS user_id,
    "storeId" AS store_id, state::text AS state, "checkInAt" AS check_in_at"#;

/// Filter Prisma pada kolom `@db.Uuid` dengan string non-UUID = P2023 → 500 (filter Nest).
pub fn prisma_uuid(raw: &str) -> ApiResult<uuid::Uuid> {
    mz_core::prisma::parse_uuid(raw).map_err(|m| ApiError::internal(anyhow::anyhow!(m)))
}

fn visit_not_found() -> ApiError {
    not_found("VISIT_NOT_FOUND", "Visit not found")
}

/// `findOwnVisit(userId, visitUuid)` (:122) — `userId` ikut di `where`; `state` TIDAK diperiksa.
pub async fn find_own_visit<'e>(
    db: impl PgExecutor<'e>,
    user_id: i32,
    visit_uuid: &str,
) -> ApiResult<VisitContext> {
    let uuid = prisma_uuid(visit_uuid)?;
    sqlx::query_as::<_, VisitContext>(&format!(
        r#"SELECT {VISIT_COLUMNS} FROM "storeVisit" WHERE uuid = $1 AND "userId" = $2 LIMIT 1"#
    ))
    .bind(uuid)
    .bind(user_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(visit_not_found)
}

/// Baca ulang kunjungan DI DALAM transaksi tulis (`runWrite`, :551) — tanpa filter `userId`;
/// pemilik diperiksa [`assert_visit_writable`].
pub async fn find_visit_for_write<'e>(
    db: impl PgExecutor<'e>,
    visit_uuid: &str,
) -> ApiResult<VisitContext> {
    let uuid = prisma_uuid(visit_uuid)?;
    sqlx::query_as::<_, VisitContext>(&format!(
        r#"SELECT {VISIT_COLUMNS} FROM "storeVisit" WHERE uuid = $1 LIMIT 1"#
    ))
    .bind(uuid)
    .fetch_optional(db)
    .await?
    .ok_or_else(visit_not_found)
}

/// `assertVisitWritable(visit, currentUserId)` (:144) — pemilik dulu, baru state.
pub fn assert_visit_writable(visit: &VisitContext, current_user_id: i32) -> ApiResult<()> {
    if visit.user_id != current_user_id {
        return Err(coded(
            StatusCode::FORBIDDEN,
            "FORM_VISIT_FORBIDDEN",
            "This visit belongs to another user",
        ));
    }
    if visit.state != ACTIVE_VISIT_STATE {
        return Err(coded(
            StatusCode::CONFLICT,
            "FORM_VISIT_NOT_ACTIVE",
            "This visit is no longer active",
        ));
    }
    Ok(())
}

/// `denormalizedColumns(tx, visit)` (:520) — snapshot write-once untuk baris submission baru.
#[derive(Debug, Clone, PartialEq)]
pub struct DenormalizedColumns {
    pub company_id: i32,
    pub visit_id: i32,
    pub visit_uuid: String,
    pub user_id: i32,
    pub store_id: i32,
    /// Nama toko SAAT kunjungan (`store?.name ?? ''`).
    pub store_name: String,
    pub visit_check_in_at: NaiveDateTime,
}

pub async fn denormalized_columns<'e>(
    db: impl PgExecutor<'e>,
    visit: &VisitContext,
) -> ApiResult<DenormalizedColumns> {
    let store_name: Option<String> = sqlx::query_scalar(r#"SELECT name FROM store WHERE id = $1"#)
        .bind(visit.store_id)
        .fetch_optional(db)
        .await?;
    Ok(DenormalizedColumns {
        company_id: visit.company_id,
        visit_id: visit.id,
        visit_uuid: visit.uuid.clone(),
        user_id: visit.user_id,
        store_id: visit.store_id,
        store_name: store_name.unwrap_or_default(),
        visit_check_in_at: visit.check_in_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visit(user_id: i32, state: &str) -> VisitContext {
        VisitContext {
            id: 1,
            uuid: "v".into(),
            company_id: 1,
            user_id,
            store_id: 1,
            state: state.into(),
            check_in_at: NaiveDateTime::default(),
        }
    }

    fn code(e: ApiError) -> (u16, String) {
        let status = e.status().as_u16();
        let body = match e {
            ApiError::Body { body, .. } => body["code"].as_str().unwrap().to_owned(),
            other => panic!("{other:?}"),
        };
        (status, body)
    }

    // spec: assertVisitWritable (§6.1)
    #[test]
    fn rejects_other_users_visit() {
        let e = assert_visit_writable(&visit(2, "active"), 1).unwrap_err();
        assert_eq!(code(e), (403, "FORM_VISIT_FORBIDDEN".into()));
    }

    #[test]
    fn passes_own_active_visit() {
        assert!(assert_visit_writable(&visit(1, "active"), 1).is_ok());
    }

    #[test]
    fn checks_owner_before_state() {
        let e = assert_visit_writable(&visit(2, "completed"), 1).unwrap_err();
        assert_eq!(code(e), (403, "FORM_VISIT_FORBIDDEN".into()));
        let e = assert_visit_writable(&visit(1, "completed"), 1).unwrap_err();
        assert_eq!(code(e), (409, "FORM_VISIT_NOT_ACTIVE".into()));
    }

    #[test]
    fn invalid_uuid_is_internal() {
        assert_eq!(prisma_uuid("nope").unwrap_err().status().as_u16(), 500);
        assert!(prisma_uuid("c0ffee00-1111-4222-8333-444455556666").is_ok());
    }
}
