//! Port `src/overtime/overtime-settings.service.ts`: setelan lembur per company + daftar
//! approver (specs/004 T031/T033, research R6). Baris `companyOvertimeSetting` dibuat malas
//! lewat [`ensure_settings`]; approver efektif dihitung saat dibaca, tick memangkas sisanya
//! (`crate::prune::prune_approvers`).
//!
//! Bagian yang dipakai service lain: [`ensure_settings`], [`effective_approvers`],
//! [`is_effective_approver`], [`effective_approver_count`], konstanta bawaan
//! (`super::constants`). Sisanya logika route `GET/PUT /v1/overtime/settings*` dan
//! `GET /v1/overtime/status`.

use std::sync::Arc;

use axum::http::StatusCode;
use chrono::{NaiveDate, NaiveDateTime};
use mz_audit::{AuditHandle, Lookup, MemberEffect, Model, Row, Touched};
use mz_core::company::require_company_id;
use mz_core::enums::{PermissionAction, Role, rank_of};
use mz_core::permissions::{EffectiveAccess, effective_access};
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState};
use serde::Serialize;
use serde_json::json;
use sqlx::{PgConnection, PgExecutor, PgPool};

use super::clock::{company_time_zone_or_utc, current_off_days};
use super::constants::{
    DEFAULT_OVERTIME_ENABLED, DEFAULT_RETRO_WINDOW_DAYS, LEGAL_NOTE, MAX_RETRO_WINDOW_DAYS,
    MIN_RETRO_WINDOW_DAYS,
};
use super::decidable::push_decidable_where;
use super::dto::is_uuid_any;
use super::errors::overtime_error;
use crate::locks::lock_settings_for_update;
use crate::zone::{local_date_key, ms_of};

// ── potongan SQL approver efektif ───────────────────────────────────────────

/// `APPROVER_ROLE_WHERE` untuk alias `al` (`accessLevel`): SUPER_ADMIN/ADMIN/MANAGER
/// bawaan, atau role custom AKTIF ber-`baseRole` ADMIN/MANAGER (`roleCodeFilter` +
/// `{ OR: [{ isSystem: true }, { status: 1 }] }`).
macro_rules! approver_role_sql {
    () => {
        r#"((al."isSystem" = true AND al.code IN ('SUPER_ADMIN', 'ADMIN', 'MANAGER'))
            OR (al."isSystem" = false AND al."baseRole" IN ('ADMIN', 'MANAGER')))
           AND (al."isSystem" = true OR al.status = 1)"#
    };
}

/// `effectiveApproverUserWhere(companyId)` untuk alias `u` (`users`), company = `$1`:
/// user aktif, keanggotaan aktif di company, dan penugasan aktif ber-role approver.
/// Dipakai kandidat, daftar efektif, dan prune, jadi ketiganya tak bisa berbeda.
pub const EFFECTIVE_APPROVER_USER_SQL: &str = concat!(
    r#"u.status = 1
  AND EXISTS (SELECT 1 FROM "userCompany" uc
              WHERE uc."userId" = u.id AND uc."companyId" = $1 AND uc.status = 1)
  AND EXISTS (SELECT 1 FROM "userAccessLevel" ual JOIN "accessLevel" al ON al.id = ual."accessLevelId"
              WHERE ual."userId" = u.id AND ual."companyId" = $1 AND ual.status = 1 AND "#,
    approver_role_sql!(),
    ")"
);

/// [`EFFECTIVE_APPROVER_USER_SQL`] dengan company dari ekspresi SQL lain, bukan `$1`
/// (dipakai `prune` di dalam `QueryBuilder`, yang nomor bind-nya tidak tetap).
pub fn effective_approver_user_sql_for(company_expr: &str) -> String {
    EFFECTIVE_APPROVER_USER_SQL.replace("$1", company_expr)
}

/// Kolom approver (`approverUserSelect` → `toEffectiveApprover`) untuk alias `u`,
/// company = `$1`: role = kode efektif penugasan approver aktif ber-id terkecil.
const APPROVER_COLUMNS: &str = concat!(
    r#"u.id, u.uuid::text AS uuid, u.name, u.username AS email,
  COALESCE((SELECT CASE WHEN al."isSystem" THEN al.code ELSE COALESCE(al."baseRole"::text, al.code) END
            FROM "userAccessLevel" ual JOIN "accessLevel" al ON al.id = ual."accessLevelId"
            WHERE ual."userId" = u.id AND ual."companyId" = $1 AND ual.status = 1 AND "#,
    approver_role_sql!(),
    r#"
            ORDER BY ual.id ASC LIMIT 1), '') AS role"#
);

/// `EffectiveApprover`.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct EffectiveApprover {
    /// `users.id`.
    pub id: i32,
    pub uuid: String,
    pub name: String,
    /// Email login (`users.username`).
    pub email: String,
    /// Kode role efektif (role custom → `baseRole`).
    pub role: String,
}

/// `ApproverView` / `ApproverEntity`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApproverView {
    /// `users.uuid`.
    pub user_id: String,
    pub name: String,
    pub email: String,
    pub role: String,
}

impl From<&EffectiveApprover> for ApproverView {
    fn from(a: &EffectiveApprover) -> Self {
        Self {
            user_id: a.uuid.clone(),
            name: a.name.clone(),
            email: a.email.clone(),
            role: a.role.clone(),
        }
    }
}

/// `effectiveApprovers(client, companyId)`: approver efektif, dihitung saat dibaca (R6),
/// urut nama lalu `overtimeApprover.id`. `exec` boleh pool atau `&mut *tx` (T9).
pub async fn effective_approvers<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
) -> sqlx::Result<Vec<EffectiveApprover>> {
    sqlx::query_as(&format!(
        r#"SELECT {APPROVER_COLUMNS}
           FROM "overtimeApprover" oa JOIN users u ON u.id = oa."userId"
           WHERE oa."companyId" = $1 AND {EFFECTIVE_APPROVER_USER_SQL}
           ORDER BY u.name ASC, oa.id ASC"#
    ))
    .bind(company_id)
    .fetch_all(exec)
    .await
}

/// `isEffectiveApprover(client, companyId, userId)` (T9: jalur decide memanggilnya dengan tx).
pub async fn is_effective_approver<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
    user_id: i32,
) -> sqlx::Result<bool> {
    sqlx::query_scalar(&format!(
        r#"SELECT EXISTS (SELECT 1 FROM "overtimeApprover" oa JOIN users u ON u.id = oa."userId"
                          WHERE oa."companyId" = $1 AND oa."userId" = $2
                            AND {EFFECTIVE_APPROVER_USER_SQL})"#
    ))
    .bind(company_id)
    .bind(user_id)
    .fetch_one(exec)
    .await
}

/// `overtimeApprover.count({ where: effectiveApproverWhere(companyId) })`.
pub async fn effective_approver_count<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
) -> sqlx::Result<i64> {
    sqlx::query_scalar(&format!(
        r#"SELECT COUNT(*) FROM "overtimeApprover" oa JOIN users u ON u.id = oa."userId"
           WHERE oa."companyId" = $1 AND {EFFECTIVE_APPROVER_USER_SQL}"#
    ))
    .bind(company_id)
    .fetch_one(exec)
    .await
}

// ── ensureSettings ───────────────────────────────────────────────────────────

/// `ensureSettings(companyId)`: membuat baris setelan bawaan + men-seed owner sebagai
/// approver, SATU transaksi (R6, H1). Hanya pemanggil yang INSERT-nya mengembalikan baris
/// yang men-seed. Owner = `company.ownerUserId` bila ia SUPER_ADMIN bawaan aktif & anggota
/// aktif (spec 007 R-24), selain itu SUPER_ADMIN aktif tertua (createdAt, id). Tanpa audit
/// (statement raw di Node: default yang di-seed bukan "perubahan", FR-009).
pub async fn ensure_settings(db: &PgPool, company_id: i32) -> ApiResult<()> {
    let existing: Option<i32> =
        sqlx::query_scalar(r#"SELECT id FROM "companyOvertimeSetting" WHERE "companyId" = $1"#)
            .bind(company_id)
            .fetch_optional(db)
            .await?;
    if existing.is_some() {
        return Ok(());
    }

    let mut tx = db.begin().await?;
    let inserted: Option<i32> = sqlx::query_scalar(
        r#"INSERT INTO "companyOvertimeSetting" ("companyId", "enabled", "retroWindowDays", "createdAt", "updatedAt")
           VALUES ($1, true, $2, now(), now())
           ON CONFLICT ("companyId") DO NOTHING
           RETURNING id"#,
    )
    .bind(company_id)
    .bind(DEFAULT_RETRO_WINDOW_DAYS)
    .fetch_optional(&mut *tx)
    .await?;
    if inserted.is_none() {
        // Pemanggil lain sudah membuat + men-seed.
        tx.commit().await?;
        return Ok(());
    }

    // SUPER_ADMIN tak bisa jadi role custom: `roleCodeFilter([SUPER_ADMIN])` = cabang isSystem.
    const OWNER_WHERE: &str = r#"ual."companyId" = $1 AND ual.status = 1
        AND al."isSystem" = true AND al.code IN ('SUPER_ADMIN')
        AND u.status = 1
        AND EXISTS (SELECT 1 FROM "userCompany" uc
                    WHERE uc."userId" = u.id AND uc."companyId" = $1 AND uc.status = 1)"#;
    let owner_user_id: Option<i32> =
        sqlx::query_scalar::<_, Option<i32>>(r#"SELECT "ownerUserId" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
    let mut owner: Option<i32> = None;
    if let Some(owner_user_id) = owner_user_id {
        owner = sqlx::query_scalar(&format!(
            r#"SELECT ual."userId" FROM "userAccessLevel" ual
               JOIN "accessLevel" al ON al.id = ual."accessLevelId"
               JOIN users u ON u.id = ual."userId"
               WHERE {OWNER_WHERE} AND ual."userId" = $2
               LIMIT 1"#
        ))
        .bind(company_id)
        .bind(owner_user_id)
        .fetch_optional(&mut *tx)
        .await?;
    }
    if owner.is_none() {
        owner = sqlx::query_scalar(&format!(
            r#"SELECT ual."userId" FROM "userAccessLevel" ual
               JOIN "accessLevel" al ON al.id = ual."accessLevelId"
               JOIN users u ON u.id = ual."userId"
               WHERE {OWNER_WHERE}
               ORDER BY ual."createdAt" ASC, ual.id ASC
               LIMIT 1"#
        ))
        .bind(company_id)
        .fetch_optional(&mut *tx)
        .await?;
    }
    if let Some(owner) = owner {
        sqlx::query(
            r#"INSERT INTO "overtimeApprover" ("companyId", "userId", "createdAt", "createdById")
               VALUES ($1, $2, now(), NULL)
               ON CONFLICT ("companyId", "userId") DO NOTHING"#,
        )
        .bind(company_id)
        .bind(owner)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Baca `enabled`/`retroWindowDays` (tanpa lock); `None` = belum ada baris.
pub async fn read_settings_row<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
) -> sqlx::Result<Option<(bool, i32)>> {
    sqlx::query_as(
        r#"SELECT "enabled", "retroWindowDays" FROM "companyOvertimeSetting" WHERE "companyId" = $1"#,
    )
    .bind(company_id)
    .fetch_optional(exec)
    .await
}

// ── tampilan ─────────────────────────────────────────────────────────────────

/// `{ noApprovers }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsReminders {
    pub no_approvers: bool,
}

/// `OvertimeSettingsView` / `OvertimeSettingsEntity`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OvertimeSettingsView {
    pub enabled: bool,
    pub retro_window_days: i32,
    pub approvers: Vec<ApproverView>,
    pub reminders: SettingsReminders,
    pub legal_note: &'static str,
}

/// `buildView(companyId)`.
pub async fn build_view(db: &PgPool, company_id: i32) -> ApiResult<OvertimeSettingsView> {
    let settings = read_settings_row(db, company_id).await?;
    let approvers = effective_approvers(db, company_id).await?;
    Ok(OvertimeSettingsView {
        enabled: settings.map_or(DEFAULT_OVERTIME_ENABLED, |s| s.0),
        retro_window_days: settings.map_or(DEFAULT_RETRO_WINDOW_DAYS, |s| s.1),
        reminders: SettingsReminders {
            no_approvers: approvers.is_empty(),
        },
        approvers: approvers.iter().map(ApproverView::from).collect(),
        legal_note: LEGAL_NOTE,
    })
}

/// `getSettings(userId)`.
pub async fn get_settings(state: &AppState, user_id: i32) -> ApiResult<OvertimeSettingsView> {
    let company_id = require_company_id(&state.db, user_id).await?;
    ensure_settings(&state.db, company_id).await?;
    build_view(&state.db, company_id).await
}

/// `approverCandidates(userId, search?)`: anggota aktif ber-role approver, urut nama lalu
/// id, maks. 200. `search` mencocokkan nama atau email (`ILIKE`).
pub async fn approver_candidates(
    state: &AppState,
    user_id: i32,
    search: Option<&str>,
) -> ApiResult<Vec<ApproverView>> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let term: String = search
        .map(|s| super::dto::js_trim(s).chars().take(100).collect())
        .unwrap_or_default();
    let search_sql = if term.is_empty() {
        ""
    } else {
        " AND (u.name ILIKE $3 OR u.username ILIKE $3)"
    };
    let sql = format!(
        r#"SELECT {APPROVER_COLUMNS} FROM users u
           WHERE {EFFECTIVE_APPROVER_USER_SQL}{search_sql}
           ORDER BY u.name ASC, u.id ASC
           LIMIT $2"#
    );
    let mut q = sqlx::query_as::<_, EffectiveApprover>(&sql)
        .bind(company_id)
        .bind(super::constants::APPROVER_CANDIDATE_LIMIT);
    if !term.is_empty() {
        q = q.bind(format!("%{term}%"));
    }
    let rows = q.fetch_all(&state.db).await?;
    Ok(rows.iter().map(ApproverView::from).collect())
}

// ── disable impact ───────────────────────────────────────────────────────────

/// `DisableImpactView`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DisableImpactView {
    /// PENDING_APPROVAL + PENDING_EMPLOYEE.
    pub pending: i64,
    /// APPROVED dengan `date ≥ localToday` (hari ini ikut).
    pub approved: i64,
    pub total: i64,
}

/// `disableCancelWhere(companyId, localToday).all` (R8/FR-034) untuk tabel tanpa alias,
/// company = `$1`, tanggal lokal = `$2`: SATU predikat untuk pratinjau, hitung ulang, dan
/// pembatalan. APPROVED lampau dan COMPLETED tak pernah cocok.
pub const DISABLE_PENDING_SQL: &str =
    r#""companyId" = $1 AND status IN ('PENDING_APPROVAL', 'PENDING_EMPLOYEE')"#;
pub const DISABLE_APPROVED_SQL: &str = r#""companyId" = $1 AND status = 'APPROVED' AND date >= $2"#;

/// `countDisableImpact`: zona company null/tak dikenal → UTC. Company tak ada → 404.
async fn count_disable_impact(
    conn: &mut PgConnection,
    company_id: i32,
    now: NaiveDateTime,
) -> ApiResult<(DisableImpactView, NaiveDate)> {
    let tz: Option<String> = sqlx::query_scalar(r#"SELECT "timeZone" FROM company WHERE id = $1"#)
        .bind(company_id)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(tz) = tz else {
        return Err(ApiError::not_found("Company not found"));
    };
    let local_today = local_date_key(ms_of(now), Some(&tz));
    let pending: i64 = sqlx::query_scalar(&format!(
        r#"SELECT COUNT(*) FROM "overtimeRequest" WHERE {DISABLE_PENDING_SQL}"#
    ))
    .bind(company_id)
    .fetch_one(&mut *conn)
    .await?;
    let approved: i64 = sqlx::query_scalar(&format!(
        r#"SELECT COUNT(*) FROM "overtimeRequest" WHERE {DISABLE_APPROVED_SQL}"#
    ))
    .bind(company_id)
    .bind(local_today)
    .fetch_one(&mut *conn)
    .await?;
    Ok((
        DisableImpactView {
            pending,
            approved,
            total: pending + approved,
        },
        local_today,
    ))
}

/// `disableImpact(userId)` (FR-035/SC-005): pratinjau jumlah yang AKAN dibatalkan.
pub async fn disable_impact(state: &AppState, user_id: i32) -> ApiResult<DisableImpactView> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let mut conn = state.db.acquire().await?;
    Ok(count_disable_impact(&mut conn, company_id, now_naive())
        .await?
        .0)
}

// ── updateSettings ───────────────────────────────────────────────────────────

/// `UpdateOvertimeSettingsInput` (sesudah validasi DTO).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateOvertimeSettingsInput {
    pub enabled: Option<bool>,
    /// `total` dari `GET /disable-impact`; wajib hanya saat true → false (T6).
    pub expected_cancel_count: Option<i64>,
    pub retro_window_days: Option<i64>,
    /// `users.uuid`; menggantikan seluruh himpunan approver.
    pub approver_user_ids: Option<Vec<String>>,
}

/// `UpdateOvertimeSettingsResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOvertimeSettingsResult {
    pub view: OvertimeSettingsView,
    /// Ada HANYA bila panggilan ini mematikan fitur.
    pub cancelled: Option<u64>,
}

/// Lapis kedua validasi service (DTO sudah menolak dengan 400).
fn check_input(input: &UpdateOvertimeSettingsInput) -> ApiResult<()> {
    if input.expected_cancel_count.is_some_and(|n| n < 0) {
        return Err(ApiError::bad_request(
            "expectedCancelCount harus bilangan bulat ≥ 0.",
        ));
    }
    if input.retro_window_days.is_some_and(|n| {
        n < i64::from(MIN_RETRO_WINDOW_DAYS) || n > i64::from(MAX_RETRO_WINDOW_DAYS)
    }) {
        return Err(ApiError::bad_request(format!(
            "retroWindowDays harus bilangan bulat {MIN_RETRO_WINDOW_DAYS}–{MAX_RETRO_WINDOW_DAYS}."
        )));
    }
    Ok(())
}

/// `[...new Set(uuids)]` — urutan kemunculan pertama dipertahankan.
pub fn dedupe_preserving_order(uuids: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(uuids.len());
    for u in uuids {
        if !out.contains(u) {
            out.push(u.clone());
        }
    }
    out
}

/// `updateSettings(userId, input)`. Satu transaksi di bawah `FOR UPDATE` (T5/T6):
/// mematikan fitur (true → false) mensyaratkan `expectedCancelCount` = hitungan ulang di
/// dalam tx (beda → 409, tak ada yang ditulis); approver diganti; `enabled`/
/// `retroWindowDays` ditulis dalam SATU update ber-audit; pembatalan `FEATURE_DISABLED`
/// tanpa email dan tanpa audit (`overtimeRequest` tidak diaudit Node).
pub async fn update_settings(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    input: &UpdateOvertimeSettingsInput,
) -> ApiResult<UpdateOvertimeSettingsResult> {
    let company_id = require_company_id(&state.db, user_id).await?;
    check_input(input)?;
    let approver_uuids = input
        .approver_user_ids
        .as_deref()
        .map(dedupe_preserving_order);

    ensure_settings(&state.db, company_id).await?;

    let mut tx = state.db.begin().await?;
    let discard_on_error = audit.discard_guard();
    let Some(locked) = lock_settings_for_update(&mut tx, company_id).await? else {
        return Err(ApiError::not_found("Company not found"));
    };
    let at = now_naive();

    let disabling = input.enabled == Some(false) && locked.enabled;
    let enabled_changed = input.enabled.is_some_and(|e| e != locked.enabled);

    let mut cancel_today: Option<NaiveDate> = None;
    if disabling {
        let Some(expected) = input.expected_cancel_count else {
            return Err(overtime_error(
                StatusCode::BAD_REQUEST,
                "OVERTIME_EXPECTED_CANCEL_COUNT_REQUIRED",
                "Konfirmasi jumlah lembur yang akan dibatalkan (expectedCancelCount) wajib dikirim saat menonaktifkan fitur lembur.",
                None,
            ));
        };
        let (fresh, local_today) = count_disable_impact(&mut tx, company_id, at).await?;
        if fresh.total != expected {
            return Err(overtime_error(
                StatusCode::CONFLICT,
                "OVERTIME_DISABLE_IMPACT_CHANGED",
                "Jumlah lembur yang akan dibatalkan sudah berubah. Periksa ulang lalu simpan lagi.",
                Some(json!({
                    "pending": fresh.pending,
                    "approved": fresh.approved,
                    "total": fresh.total,
                })),
            ));
        }
        cancel_today = Some(local_today);
    }

    if let Some(uuids) = &approver_uuids {
        replace_approvers(&mut tx, audit, company_id, user_id, uuids).await?;
    }

    let retro = input
        .retro_window_days
        .and_then(|r| i32::try_from(r).ok())
        .filter(|r| *r != locked.retro_window_days);
    if enabled_changed || retro.is_some() {
        // SATU update (bukan dua) supaya audit mencatat `enabled` dan `retroWindowDays`
        // lama/baru dalam satu baris (FR-009). false → true hanya membalik flag (FR-037).
        let lookup = move || {
            Lookup::filter(move |qb| {
                qb.push(r#""companyId" = "#).push_bind(company_id);
            })
        };
        let pre = audit
            .pre_read(Model::CompanyOvertimeSetting, lookup())
            .await;
        let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(
            r#"UPDATE "companyOvertimeSetting" SET "updatedById" = "#,
        );
        qb.push_bind(user_id)
            .push(r#", "updatedAt" = "#)
            .push_bind(at);
        if enabled_changed && let Some(enabled) = input.enabled {
            qb.push(r#", "enabled" = "#).push_bind(enabled);
        }
        if let Some(retro) = retro {
            qb.push(r#", "retroWindowDays" = "#).push_bind(retro);
        }
        qb.push(r#" WHERE "companyId" = "#).push_bind(company_id);
        qb.build().execute(&mut *tx).await?;
        if let Some(row) = audit
            .result_row(&mut *tx, Model::CompanyOvertimeSetting, lookup())
            .await
        {
            audit
                .record_update(Model::CompanyOvertimeSetting, pre, &row)
                .await;
        }
    }

    let mut cancelled = None;
    if let Some(local_today) = cancel_today {
        // FR-034: pembatalan sistem — `cancelledById` null, tanpa email. READ COMMITTED
        // mengevaluasi ulang WHERE, jadi baris yang dibatalkan tick lebih dulu tak tertimpa.
        let n = sqlx::query(&format!(
            r#"UPDATE "overtimeRequest"
               SET status = 'CANCELLED', "cancelReason" = 'FEATURE_DISABLED',
                   "cancelledAt" = $3, "cancelledById" = NULL, "updatedAt" = $3
               WHERE ({DISABLE_PENDING_SQL}) OR ({DISABLE_APPROVED_SQL})"#
        ))
        .bind(company_id)
        .bind(local_today)
        .bind(at)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        cancelled = Some(n);
    }

    tx.commit().await?;
    discard_on_error.disarm();

    Ok(UpdateOvertimeSettingsResult {
        view: build_view(&state.db, company_id).await?,
        cancelled,
    })
}

/// `replaceApprovers`: hapus yang tak ada di set baru, buat yang belum ada; keduanya
/// diaudit per baris (keanggotaan `overtimeApprover`). Kelayakan dibaca di dalam tx.
async fn replace_approvers(
    tx: &mut PgConnection,
    audit: &AuditHandle,
    company_id: i32,
    actor_id: i32,
    uuids: &[String],
) -> ApiResult<()> {
    let well_formed: Vec<String> = uuids.iter().filter(|u| is_uuid_any(u)).cloned().collect();
    let eligible: Vec<(i32, String)> = if well_formed.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(&format!(
            r#"SELECT u.id, u.uuid::text FROM users u
               WHERE u.uuid = ANY($2::text[]::uuid[]) AND {EFFECTIVE_APPROVER_USER_SQL}"#
        ))
        .bind(company_id)
        .bind(&well_formed)
        .fetch_all(&mut *tx)
        .await?
    };
    let eligible_uuids: Vec<String> = eligible.iter().map(|(_, u)| u.to_lowercase()).collect();
    let rejected: Vec<&String> = uuids
        .iter()
        .filter(|u| !eligible_uuids.contains(&u.to_lowercase()))
        .collect();
    if !rejected.is_empty() {
        return Err(overtime_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "OVERTIME_APPROVER_NOT_ELIGIBLE",
            "Approver harus anggota aktif dengan role Super Admin, Admin, atau Manager.",
            Some(json!({ "userIds": rejected })),
        ));
    }

    let keep_ids: Vec<i32> = eligible.iter().map(|(id, _)| *id).collect();
    let keep: Arc<Vec<i32>> = Arc::new(keep_ids.clone());
    let membership = audit
        .membership_pre(
            Model::OvertimeApprover,
            MemberEffect::Remove,
            Touched::Where(Lookup::filter(move |qb| {
                qb.push(r#""companyId" = "#)
                    .push_bind(company_id)
                    .push(r#" AND NOT ("userId" = ANY("#)
                    .push_bind(keep.as_ref().clone())
                    .push("))");
            })),
        )
        .await;
    sqlx::query(
        r#"DELETE FROM "overtimeApprover" WHERE "companyId" = $1 AND NOT ("userId" = ANY($2))"#,
    )
    .bind(company_id)
    .bind(&keep_ids)
    .execute(&mut *tx)
    .await?;
    audit.record_membership(membership).await;

    if keep_ids.is_empty() {
        return Ok(());
    }
    let have: Vec<i32> = sqlx::query_scalar(
        r#"SELECT "userId" FROM "overtimeApprover" WHERE "companyId" = $1 AND "userId" = ANY($2)"#,
    )
    .bind(company_id)
    .bind(&keep_ids)
    .fetch_all(&mut *tx)
    .await?;
    let to_create: Vec<i32> = keep_ids
        .into_iter()
        .filter(|id| !have.contains(id))
        .collect();
    if to_create.is_empty() {
        return Ok(());
    }
    let now = now_naive();
    let rows: Vec<Row> = to_create
        .iter()
        .map(|uid| {
            Row::new()
                .with("companyId", company_id)
                .with("userId", *uid)
                .with("createdById", actor_id)
        })
        .collect();
    let membership = audit
        .membership_pre(
            Model::OvertimeApprover,
            MemberEffect::Add,
            Touched::Rows(rows),
        )
        .await;
    sqlx::query(
        r#"INSERT INTO "overtimeApprover" ("companyId", "userId", "createdAt", "createdById")
           SELECT $1, u, $3, $4 FROM UNNEST($2::int[]) AS t(u)"#,
    )
    .bind(company_id)
    .bind(&to_create)
    .bind(now)
    .bind(actor_id)
    .execute(&mut *tx)
    .await?;
    audit.record_membership(membership).await;
    Ok(())
}

// ── getStatus ────────────────────────────────────────────────────────────────

/// Gerbang endpoint yang dicerminkan flag `getStatus` (T119, FR-051): `@Roles` +
/// `@RequireFeature` controller-nya, verb dari method HTTP, dan aksi fitur di registry.
#[derive(Debug, Clone, Copy)]
pub struct OvertimeEndpointGate {
    pub roles: &'static [Role],
    pub feature: &'static str,
    pub action: PermissionAction,
}

/// `@Roles` laporan lembur.
pub const OVERTIME_REPORT_ROLES: &[Role] = &[
    Role::SYSTEM_ADMIN,
    Role::SUPER_ADMIN,
    Role::ADMIN,
    Role::MANAGER,
];
/// `@Roles` pengaturan lembur (MANAGER SENGAJA tidak ada — specs/004 T003b, FR-008).
pub const OVERTIME_SETTINGS_ROLES: &[Role] = &[Role::SYSTEM_ADMIN, Role::SUPER_ADMIN, Role::ADMIN];

/// `GET /v1/overtime/reports*`.
pub const OVERTIME_REPORT_GATE: OvertimeEndpointGate = OvertimeEndpointGate {
    roles: OVERTIME_REPORT_ROLES,
    feature: "overtime-report",
    action: PermissionAction::READ,
};

/// `PUT /v1/overtime/settings`.
pub const OVERTIME_SETTINGS_GATE: OvertimeEndpointGate = OvertimeEndpointGate {
    roles: OVERTIME_SETTINGS_ROLES,
    feature: "overtime-settings",
    action: PermissionAction::WRITE,
};

/// `rolesGuardAllows(access, roles)`: tanpa role aktif → tolak; role bawaan → `code` ada
/// di `@Roles`; role custom → plafon `baseRole` ≥ salah satu role di `@Roles`.
pub fn roles_guard_allows(access: Option<&EffectiveAccess>, roles: &[Role]) -> bool {
    let Some(access) = access else { return false };
    if access.is_system {
        return roles.iter().any(|r| r.as_str() == access.role_code);
    }
    let Some(ceiling) = access.base_role.and_then(|b| rank_of(b.as_str())) else {
        return false;
    };
    roles
        .iter()
        .any(|r| rank_of(r.as_str()).is_some_and(|needed| needed <= ceiling))
}

/// `grantedActionsFor` (`me-permissions.ts`) untuk fitur gerbang; daftar aksi diturunkan dari
/// `mz_core::feature_registry` lewat [`mz_core::permissions::granted_actions`].
pub fn granted_actions(
    access: Option<&EffectiveAccess>,
    gate: &OvertimeEndpointGate,
) -> Vec<PermissionAction> {
    mz_core::permissions::granted_actions(access, gate.feature)
}

/// Keputusan gerbang yang benar-benar berlaku (RolesGuard + matriks izin).
pub fn gate_allows(access: Option<&EffectiveAccess>, gate: &OvertimeEndpointGate) -> bool {
    roles_guard_allows(access, gate.roles) && granted_actions(access, gate).contains(&gate.action)
}

/// `OvertimeStatusView` / `OvertimeStatusEntity`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OvertimeStatusView {
    pub enabled: bool,
    pub is_approver: bool,
    pub can_view_report: bool,
    pub can_manage_settings: bool,
    pub approver_count: i64,
    /// PENDING_APPROVAL yang bisa diputuskan user ini; 0 bila bukan approver.
    pub pending_approval_count: i64,
    /// ORDER PENDING_EMPLOYEE milik user ini yang belum lewat tenggat.
    pub pending_order_count: i64,
    pub legal_note: &'static str,
    pub time_zone: String,
    pub retro_window_days: i32,
    pub weekly_off_days: Vec<i32>,
}

/// `getStatus(userId)`.
pub async fn get_status(state: &AppState, user_id: i32) -> ApiResult<OvertimeStatusView> {
    let db = &state.db;
    let company_id = require_company_id(db, user_id).await?;
    ensure_settings(db, company_id).await?;

    let now = now_naive();
    let settings = read_settings_row(db, company_id).await?;
    let tz: Option<String> = sqlx::query_scalar(r#"SELECT "timeZone" FROM company WHERE id = $1"#)
        .bind(company_id)
        .fetch_optional(db)
        .await?;
    let access = effective_access(state, user_id, company_id).await?;
    let is_approver = is_effective_approver(db, company_id, user_id).await?;
    let approver_count = effective_approver_count(db, company_id).await?;

    // Predikat sama dengan daftar Menunggu; approver tunggal boleh memutuskan pengajuannya
    // sendiri. Bukan approver → 0, tanpa query.
    let pending_approval_count = if is_approver {
        let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(
            r#"SELECT COUNT(*) FROM "overtimeRequest" WHERE "#,
        );
        push_decidable_where(&mut qb, "", company_id, user_id, approver_count < 2, now);
        qb.build_query_scalar::<i64>().fetch_one(db).await?
    } else {
        0
    };
    let pending_order_count: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "overtimeRequest"
           WHERE "companyId" = $1 AND "userId" = $2 AND kind = 'ORDER'
             AND status = 'PENDING_EMPLOYEE' AND "responseDeadline" > $3"#,
    )
    .bind(company_id)
    .bind(user_id)
    .bind(now)
    .fetch_one(db)
    .await?;
    let weekly_off_days = {
        let mut conn = db.acquire().await?;
        current_off_days(&mut conn, company_id, tz.as_deref(), now).await?
    };

    let access = access.as_deref();
    Ok(OvertimeStatusView {
        enabled: settings.map_or(DEFAULT_OVERTIME_ENABLED, |s| s.0),
        is_approver,
        can_view_report: gate_allows(access, &OVERTIME_REPORT_GATE),
        can_manage_settings: gate_allows(access, &OVERTIME_SETTINGS_GATE),
        approver_count,
        pending_approval_count,
        pending_order_count,
        legal_note: LEGAL_NOTE,
        time_zone: company_time_zone_or_utc(tz.as_deref()),
        retro_window_days: settings.map_or(DEFAULT_RETRO_WINDOW_DAYS, |s| s.1),
        weekly_off_days,
    })
}

#[cfg(test)]
mod tests {
    //! Kasus murni `overtime-settings.service.spec.ts` (gerbang `getStatus`, dedupe, batas).
    #![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use mz_core::enums::BaseRole;
    use std::collections::{HashMap, HashSet};

    fn access(
        code: &str,
        is_system: bool,
        base: Option<BaseRole>,
        grants: &[(&str, PermissionAction)],
    ) -> EffectiveAccess {
        let mut permissions: HashMap<String, HashSet<PermissionAction>> = HashMap::new();
        for (f, a) in grants {
            permissions.entry((*f).to_owned()).or_default().insert(*a);
        }
        EffectiveAccess {
            role_id: 1,
            role_code: code.into(),
            role_name: code.into(),
            is_system,
            base_role: base,
            effective_role_code: mz_core::enums::effective_role_code(code, is_system, base),
            company_id: 1,
            permissions,
        }
    }

    const READ_REPORT: (&str, PermissionAction) = ("overtime-report", PermissionAction::READ);
    const READ_SETTINGS: (&str, PermissionAction) = ("overtime-settings", PermissionAction::READ);
    const WRITE_SETTINGS: (&str, PermissionAction) = ("overtime-settings", PermissionAction::WRITE);
    const READ_OVERTIME: (&str, PermissionAction) = ("overtime", PermissionAction::READ);
    const ALL: &[(&str, PermissionAction)] =
        &[READ_OVERTIME, READ_REPORT, READ_SETTINGS, WRITE_SETTINGS];

    fn flags(a: Option<&EffectiveAccess>) -> (bool, bool) {
        (
            gate_allows(a, &OVERTIME_REPORT_GATE),
            gate_allows(a, &OVERTIME_SETTINGS_GATE),
        )
    }

    #[test]
    fn built_in_roles_empty_matrix() {
        for (code, report, settings) in [
            ("SUPER_ADMIN", true, true),
            ("ADMIN", true, true),
            ("MANAGER", true, false),
            ("STAFF", false, false),
            ("GUEST", false, false),
        ] {
            assert_eq!(
                flags(Some(&access(code, true, None, &[]))),
                (report, settings),
                "{code}"
            );
        }
        assert_eq!(flags(None), (false, false));
    }

    #[test]
    fn custom_role_ceiling_and_matrix() {
        for (base, report, settings) in [
            (BaseRole::ADMIN, true, true),
            (BaseRole::MANAGER, true, false),
            (BaseRole::STAFF, false, false),
        ] {
            assert_eq!(
                flags(Some(&access("c1_x", false, Some(base), ALL))),
                (report, settings)
            );
        }
        let a = access(
            "c1_x",
            false,
            Some(BaseRole::ADMIN),
            &[READ_OVERTIME, READ_SETTINGS, WRITE_SETTINGS],
        );
        assert_eq!(flags(Some(&a)), (false, true));
        let a = access(
            "c1_x",
            false,
            Some(BaseRole::ADMIN),
            &[READ_REPORT, READ_SETTINGS],
        );
        assert_eq!(flags(Some(&a)), (true, false));
        let a = access("c1_x", false, Some(BaseRole::ADMIN), &[]);
        assert_eq!(flags(Some(&a)), (false, false));
        // Custom tanpa baseRole → plafon tak dikenal.
        assert_eq!(
            flags(Some(&access("c1_x", false, None, ALL))),
            (false, false)
        );
    }

    #[test]
    fn built_in_with_filled_matrix_is_narrowed() {
        let a = access("ADMIN", true, None, &[READ_OVERTIME, READ_REPORT]);
        assert_eq!(flags(Some(&a)), (true, false));
        let a = access(
            "ADMIN",
            true,
            None,
            &[READ_OVERTIME, READ_SETTINGS, WRITE_SETTINGS],
        );
        assert_eq!(flags(Some(&a)), (false, true));
    }

    #[test]
    fn gates_mirror_controllers() {
        assert_eq!(OVERTIME_REPORT_GATE.feature, "overtime-report");
        assert_eq!(OVERTIME_REPORT_GATE.action, PermissionAction::READ);
        assert_eq!(OVERTIME_SETTINGS_GATE.feature, "overtime-settings");
        assert_eq!(OVERTIME_SETTINGS_GATE.action, PermissionAction::WRITE);
        assert!(!OVERTIME_SETTINGS_GATE.roles.contains(&Role::MANAGER));
    }

    #[test]
    fn gate_actions_derive_from_registry_same_as_former_copy() {
        use PermissionAction::{READ, WRITE};
        use mz_core::feature_registry::feature_by_key;
        // Salinan lama: report = [READ], settings = [READ, WRITE]; bukan legacyOpen.
        let report = feature_by_key("overtime-report").map(|f| (f.actions, f.is_legacy_open()));
        let settings = feature_by_key("overtime-settings").map(|f| (f.actions, f.is_legacy_open()));
        assert_eq!(report, Some((&[READ][..], false)));
        assert_eq!(settings, Some((&[READ, WRITE][..], false)));
        // Role bawaan + matriks kosong → semua aksi fitur; tanpa role → kosong.
        let empty = access("ADMIN", true, None, &[]);
        assert_eq!(
            granted_actions(Some(&empty), &OVERTIME_REPORT_GATE),
            vec![READ]
        );
        assert_eq!(
            granted_actions(Some(&empty), &OVERTIME_SETTINGS_GATE),
            vec![READ, WRITE]
        );
        assert!(granted_actions(None, &OVERTIME_SETTINGS_GATE).is_empty());
    }

    #[test]
    fn dedupe_and_bounds() {
        let v: Vec<String> = ["b", "a", "b", "c", "a"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(dedupe_preserving_order(&v), vec!["b", "a", "c"]);
        for bad in [-1, 32] {
            let input = UpdateOvertimeSettingsInput {
                retro_window_days: Some(bad),
                ..Default::default()
            };
            assert!(check_input(&input).is_err());
        }
        assert!(check_input(&UpdateOvertimeSettingsInput::default()).is_ok());
    }

    #[test]
    fn sql_fragments_are_parameterised() {
        assert!(EFFECTIVE_APPROVER_USER_SQL.contains(r#"al."baseRole" IN ('ADMIN', 'MANAGER')"#));
        assert!(APPROVER_COLUMNS.contains("ORDER BY ual.id ASC LIMIT 1"));
        assert!(!DISABLE_APPROVED_SQL.contains("COMPLETED"));
    }
}
