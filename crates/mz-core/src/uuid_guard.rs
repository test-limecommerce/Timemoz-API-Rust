//! Port param decorator `@UUID(...)` — Node: `src/uuid/uuid.decorator.ts:47` (`checkUuidAccess`, dipakai
//! `UUID` di :112) dan `src/uuid/company-ownership.ts:31` (`COMPANY_OWNERSHIP`, pemetaan tabel -> company). SATU implementasi untuk semua
//! crate (dulu `mz_projects::service::ensure_project_access` dan `mz_companies::common::ensure_uuid_access`).
//!
//! Aturan (identik dengan Node):
//! 1. Company aktif pelaku = [`crate::company::find_company_id`] (memo di `RequestCtx::company_id`).
//! 2. Pelaku *elevated* HANYA bila role efektifnya DI COMPANY AKTIF itu `sees_whole_company`
//!    ([`crate::roles::role_codes_in_company`]). Role di company lain tidak dihitung (admin A yang
//!    STAFF di B bukan elevated di B).
//! 3. Elevated lolos hanya bila baris milik company aktif ([`GuardTable::push_in_company`]); baris
//!    company lain DAN uuid yang tidak ada dijawab sama: 404 `Data not found` (keberadaan uuid tidak bocor).
//! 4. Non-elevated: jalur lama (baris harus ada -> 404; user harus ada di relasi `userId` -> 403),
//!    ditambah batas company aktif ([`GuardTable::push_in_company`]).
//! 5. Tanpa company aktif tidak ada yang elevated; jatuh ke jalur non-elevated tanpa batas company.
//!
//! Tabel `setting` (guard Node di `settings.controller.ts`) sengaja tidak ada di sini: datanya global
//! (tak punya company), route-nya hanya SYSTEM_ADMIN, jadi guard Node pasti `true` (lihat
//! `plan/deviations/mz-settings.md` butir 3).

use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::enums::sees_whole_company;
use crate::{ApiError, ApiResult, AppState, RequestCtx};

/// Tabel yang dipakai `@UUID(...)` di Node (mirror `COMPANY_OWNERSHIP`, `src/uuid/company-ownership.ts:31`).
///
/// Sinkron Node↔Rust: entri `company` di `COMPANY_OWNERSHIP` (route `POST /companies/:uuid/switch`) sengaja TIDAK
/// punya padanan di sini. Rust tidak mem-port `switch` (dihapus 2026-11-17, fitur tidak dipakai); route itu
/// tetap dilayani Node. Bila `switch` di-port lagi, tambahkan kembali varian `Company`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardTable {
    /// `@UUID({ tableName: 'project', tableRelation: 'projectUser' })`: milik company lewat
    /// `projectCompany` berstatus 1.
    Project,
    /// `@UUID({ tableName: 'projectHour' })` (route jam project; tanpa relasi `userId`): milik company
    /// lewat project-nya.
    ProjectHour,
    /// `@UUID({ tableName: 'accessLevel', tableRelation: 'userAccessLevel' })`: company-nya sendiri
    /// ATAU role sistem global (`companyId IS NULL`). Role global boleh dibaca; penolakan ubah/hapus
    /// ada di service (`rejectIfSystem`, 422 `ACCESS_LEVEL_SYSTEM_IMMUTABLE`).
    AccessLevel,
}

impl GuardTable {
    const fn sql_table(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::ProjectHour => "projectHour",
            Self::AccessLevel => "accessLevel",
        }
    }

    /// `tableRelation` Node: (tabel relasi, kolom FK ke baris ini). Selalu berkunci `userId`.
    const fn relation(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Project => Some(("projectUser", "projectId")),
            Self::ProjectHour => None,
            Self::AccessLevel => Some(("userAccessLevel", "accessLevelId")),
        }
    }

    /// Fragmen `WHERE` "baris `t` milik company `company_id`" (alias tabel wajib `t`).
    pub fn push_in_company(self, qb: &mut QueryBuilder<'_, Postgres>, company_id: i32) {
        match self {
            Self::Project => {
                qb.push(r#"EXISTS (SELECT 1 FROM "projectCompany" pc WHERE pc."projectId" = t.id AND pc."companyId" = "#)
                    .push_bind(company_id)
                    .push(" AND pc.status = 1)");
            }
            Self::ProjectHour => {
                qb.push(r#"EXISTS (SELECT 1 FROM "projectCompany" pc WHERE pc."projectId" = t."projectId" AND pc."companyId" = "#)
                    .push_bind(company_id)
                    .push(" AND pc.status = 1)");
            }
            Self::AccessLevel => {
                qb.push(r#"(t."companyId" = "#)
                    .push_bind(company_id)
                    .push(r#" OR t."companyId" IS NULL)"#);
            }
        }
    }
}

fn not_found() -> ApiError {
    ApiError::not_found("Data not found")
}

/// `SELECT t.id FROM <table> t WHERE t.uuid = $1 [AND <dalam company>]`.
async fn find_id(
    db: &PgPool,
    table: GuardTable,
    uuid: &str,
    in_company: Option<i32>,
) -> sqlx::Result<Option<i32>> {
    let mut qb = QueryBuilder::<Postgres>::new(format!(
        r#"SELECT t.id FROM "{}" t WHERE t.uuid = "#,
        table.sql_table()
    ));
    qb.push_bind(uuid).push("::uuid");
    if let Some(company_id) = in_company {
        qb.push(" AND ");
        table.push_in_company(&mut qb, company_id);
    }
    qb.push(" LIMIT 1");
    qb.build_query_scalar().fetch_optional(db).await
}

/// Node: `src/uuid/uuid.decorator.ts:47` (`checkUuidAccess`). `Ok(())` = guard mengembalikan `true`.
pub async fn ensure_uuid_access(
    state: &AppState,
    ctx: &RequestCtx,
    table: GuardTable,
    uuid: &str,
) -> ApiResult<()> {
    let user_id = ctx.require_user_id()?;
    let company_id = ctx.company_id(state).await?;

    let elevated = match company_id {
        Some(company) => crate::roles::role_codes_in_company(&state.db, user_id, company)
            .await?
            .iter()
            .any(|code| sees_whole_company(Some(code.as_str()))),
        None => false,
    };

    if let (true, Some(company)) = (elevated, company_id) {
        return find_id(&state.db, table, uuid, Some(company))
            .await?
            .map(|_| ())
            .ok_or_else(not_found);
    }

    let Some(id) = find_id(&state.db, table, uuid, company_id).await? else {
        return Err(not_found());
    };
    let Some((relation, fk)) = table.relation() else {
        return Err(forbidden());
    };
    let owner: Option<i32> = sqlx::query_scalar(&format!(
        r#"SELECT id FROM "{relation}" WHERE "{fk}" = $1 AND "userId" = $2 LIMIT 1"#
    ))
    .bind(id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    if owner.is_none() {
        return Err(forbidden());
    }
    Ok(())
}

fn forbidden() -> ApiError {
    ApiError::forbidden("You do not have access to this resource")
}
