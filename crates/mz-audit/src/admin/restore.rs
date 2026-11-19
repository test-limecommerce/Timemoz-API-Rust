//! Port `src/audit-logs/audit-restore.service.ts` — `POST /v1/admin/audit-logs/:uuid/restore`,
//! satu-satunya jalur tulis modul baca audit (§0.7).
//!
//! Dua hal yang menopang berkas ini (sama dengan Node):
//! 1. Baris `restore` ditulis EKSPLISIT di dalam transaksinya (§5.6) lewat konteks tulis
//!    [`AuditHandle`] (pelaku/jejak request sama dengan penulisan biasa). Tidak ada entri
//!    buffer lain yang lahir dari `UPDATE` pemulihan, jadi satu pemulihan = satu baris
//!    (padanan `discardAudit()` Node).
//! 2. Penandaan §5.5 adalah penjaga sekali-pakai: `UPDATE … WHERE restoredAt IS NULL` yang
//!    tidak mengenai satu baris pun → 409 `ALREADY_RESTORED`, dan transaksi (termasuk
//!    `UPDATE` entitas) dibatalkan.

use axum::http::StatusCode;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use mz_core::company::require_company_id;
use mz_core::{ApiError, ApiResult, AppState};
use serde_json::{Value, json};
use sqlx::PgConnection;

use super::query::{LOG_COLUMNS, LogRow, not_found};
use crate::build::AuditEntry;
use crate::handle::AuditHandle;
use crate::policy::{diff_snapshots, project_row};
use crate::read::{Columns, Lookup, read_rows};
use crate::row::Row;
use crate::scope::{Action, EntityType, Model};
use crate::sink;

/// `AUDIT_RETENTION_DAYS` (`audit-logs.service.ts`): retensi sekaligus jendela restore.
pub const AUDIT_RETENTION_DAYS: i64 = 90;
const MS_PER_DAY: i64 = 24 * 60 * 60 * 1000;

const USER_STATUS_DELETED: i32 = 0;
const USER_STATUS_DISABLED: i32 = 3;

/// §5.3 — apa yang TIDAK ikut pulih.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NotRestored {
    what: &'static str,
    reason: &'static str,
    endpoint: &'static str,
}

impl NotRestored {
    fn json(&self) -> Value {
        json!({ "what": self.what, "reason": self.reason, "endpoint": self.endpoint })
    }
}

const STORE_PATCH: &str = "PATCH /v1/admin/stores/:uuid";
const USER_PATCH: &str = "PATCH /users/:uuid";

struct Outcome {
    /// Baris entitas sesudah pemulihan, mentah — diproyeksikan pemanggil (§3.4).
    row: Row,
    warnings: Vec<String>,
    not_restored: Vec<NotRestored>,
}

impl Outcome {
    fn empty(row: Row) -> Self {
        Self {
            row,
            warnings: Vec::new(),
            not_restored: Vec::new(),
        }
    }
}

/// `RESTORE_TARGETS[entityType].model`.
pub(crate) fn model_of(entity: EntityType) -> Model {
    match entity {
        EntityType::Company => Model::Company,
        EntityType::Employee => Model::Users,
        EntityType::Store => Model::Store,
        EntityType::StoreGroup => Model::StoreGroup,
        EntityType::StoreVisitForm => Model::StoreVisitFormTemplate,
        EntityType::StoreVisitPlan => Model::StoreVisitPlan,
        EntityType::Geofence => Model::Geofence,
        EntityType::NegativeZone => Model::NegativeZone,
        EntityType::Role => Model::AccessLevel,
        EntityType::Customer => Model::Customer,
        EntityType::CustomerType => Model::CustomerType,
        EntityType::CustomerGroup => Model::CustomerGroup,
        EntityType::CustomerChannel => Model::CustomerChannel,
        EntityType::CustomerSubChannel => Model::CustomerSubChannel,
        EntityType::Product => Model::Product,
        EntityType::ProductCategory => Model::ProductCategory,
        EntityType::ProductSubCategory => Model::ProductSubCategory,
        EntityType::ProductBrand => Model::ProductBrand,
        EntityType::ProductPrincipal => Model::ProductPrincipal,
        EntityType::Uom => Model::Uom,
        EntityType::TaxRate => Model::TaxRate,
        EntityType::PriceList => Model::PriceList,
        EntityType::PriceListItem => Model::PriceListItem,
        EntityType::ProductCustomField => Model::ProductCustomField,
        EntityType::Order => Model::Order,
        EntityType::OrderType => Model::OrderType,
        EntityType::OrderReason => Model::OrderReason,
        EntityType::OrderPaymentTerm => Model::OrderPaymentTerm,
        EntityType::OrderPaymentMethod => Model::OrderPaymentMethod,
        EntityType::OrderCustomFieldDefinition => Model::OrderCustomFieldDefinition,
        EntityType::VisitType => Model::VisitType,
        EntityType::VisitFrequency => Model::VisitFrequency,
        EntityType::Division => Model::Division,
    }
}

/// §5.4 — `Serializable` hanya untuk entitas dengan unique yang bisa diserobot.
pub(crate) fn is_serializable(entity: EntityType) -> bool {
    matches!(
        entity,
        EntityType::Store
            | EntityType::StoreVisitPlan
            | EntityType::Customer
            | EntityType::Product
            | EntityType::PriceList
    )
}

fn conflict(code: &str, message: impl Into<String>, details: Option<Value>) -> ApiError {
    let mut body = json!({ "code": code, "message": message.into() });
    if let (Some(d), Some(obj)) = (details, body.as_object_mut()) {
        obj.insert("details".to_owned(), d);
    }
    ApiError::body(StatusCode::CONFLICT, body)
}

fn unprocessable(code: &str, message: &str) -> ApiError {
    ApiError::coded(StatusCode::UNPROCESSABLE_ENTITY, code, message)
}

fn millis(dt: NaiveDateTime) -> i64 {
    dt.and_utc().timestamp_millis()
}

fn already_restored(log: &LogRow) -> ApiError {
    conflict(
        "ALREADY_RESTORED",
        "This deletion has already been restored",
        Some(json!({
            "restoredAt": log.restored_at.map(millis),
            "restoredByLogId": log.restored_by_log_id,
        })),
    )
}

/// Urutannya mengikuti tabel §6.4: sudah dipulihkan menang atas kedaluwarsa.
pub(crate) fn assert_restorable(log: &LogRow, now_ms: i64) -> ApiResult<()> {
    if log.restored_at.is_some() {
        return Err(already_restored(log));
    }
    if !log.is_restorable {
        return Err(conflict(
            "NOT_RESTORABLE",
            "This audit entry cannot be restored: it is not a user deletion with a complete snapshot",
            Some(json!({ "action": log.action, "subEntity": log.sub_entity })),
        ));
    }
    let expires_at = millis(log.created_at) + AUDIT_RETENTION_DAYS * MS_PER_DAY;
    if now_ms > expires_at {
        return Err(conflict(
            "RESTORE_WINDOW_EXPIRED",
            format!("Restore window of {AUDIT_RETENTION_DAYS} days has passed"),
            Some(json!({ "createdAt": millis(log.created_at), "expiresAt": expires_at })),
        ));
    }
    Ok(())
}

fn target_missing(entity_type: &str) -> ApiError {
    conflict(
        "RESTORE_TARGET_MISSING",
        format!("The {entity_type} this entry points at no longer exists"),
        None,
    )
}

fn incomplete_snapshot(field: &str) -> ApiError {
    conflict(
        "NOT_RESTORABLE",
        format!("Snapshot is missing \"{field}\", so this entry cannot be restored"),
        Some(json!({ "field": field })),
    )
}

/// `numberFrom`: harus angka (bilangan bulat muat `Int`).
pub(crate) fn number_from(snapshot: &Value, field: &str) -> ApiResult<i32> {
    snapshot
        .get(field)
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(|| incomplete_snapshot(field))
}

/// `booleanFrom`.
pub(crate) fn bool_from(snapshot: &Value, field: &str) -> ApiResult<bool> {
    snapshot
        .get(field)
        .and_then(Value::as_bool)
        .ok_or_else(|| incomplete_snapshot(field))
}

fn entity_uuid_or_throw(log: &LogRow) -> ApiResult<&str> {
    log.entity_uuid.as_deref().ok_or_else(|| {
        conflict(
            "NOT_RESTORABLE",
            "This audit entry does not point at a restorable row",
            None,
        )
    })
}

/// Peringatan jadwal lampau (`visitDate < todayPlanDate()`, keduanya UTC).
pub(crate) fn past_plan_warning(visit_date: NaiveDate, today: NaiveDate) -> Option<String> {
    (visit_date < today).then(|| {
        format!(
            "The {} schedule is in the past; the visit can no longer be carried out",
            visit_date.format("%Y-%m-%d")
        )
    })
}

/// `changes` pada `assertNoSharedAccountChange` untuk `{ status }`.
pub(crate) fn status_change_is_blocked(actor: i32, target: i32, current: i32, next: i32) -> bool {
    actor != target && current != next
}

fn naive_from_millis(ms: i64) -> NaiveDateTime {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .unwrap_or_default()
        .naive_utc()
}

/// Entri baris `restore` §5.6.
pub(crate) fn restore_entry(
    log: &LogRow,
    entity_type: EntityType,
    company_id: i32,
    deleted_state: Option<Value>,
    restored_state: Option<Value>,
) -> AuditEntry {
    let changed = diff_snapshots(
        deleted_state.as_ref().and_then(Row::from_json).as_ref(),
        restored_state.as_ref().and_then(Row::from_json).as_ref(),
    );
    AuditEntry {
        entity_type,
        entity_id: log.entity_id,
        entity_uuid: log.entity_uuid.clone(),
        entity_label: log.entity_label.clone(),
        sub_entity: None,
        action: Action::Restore,
        company_id: Some(company_id),
        before: deleted_state,
        after: restored_state,
        changed_fields: changed,
        is_restorable: false,
    }
}

/// §6.4.
pub(crate) async fn restore(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    uuid: &str,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;

    // `companyId` di `where` sendiri (§7.3 syarat 2): uuid company lain 404.
    let log: Option<LogRow> = sqlx::query_as(&format!(
        r#"SELECT {LOG_COLUMNS} FROM "auditLog" WHERE uuid = $1::uuid AND "companyId" = $2 LIMIT 1"#
    ))
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(log) = log else {
        return Err(not_found());
    };

    assert_restorable(&log, Utc::now().timestamp_millis())?;

    let entity_type =
        EntityType::from_name(&log.entity_type).ok_or_else(|| target_missing(&log.entity_type))?;
    let model = model_of(entity_type);
    let before = log.before.clone().unwrap_or_else(|| json!({}));

    // §0.16 — region dipulihkan service yang sudah rilis (di luar transaksi di bawah):
    // `isActive = true` idempoten, jadi kegagalan penandaan sesudahnya cukup diulang.
    if entity_type == EntityType::StoreGroup {
        restore_store_group(state, company_id, entity_uuid_or_throw(&log)?).await?;
    }

    // Deviasi: Node tidak me-retry, jadi P2034 jatuh ke 500. Di sini konflik serialisasi/
    // deadlock diulang seperti `retryTransientDbError` (3 percobaan, 150 ms × 2^(n-1) +
    // jitter). Aman: percobaan yang gagal sudah di-rollback, dan penanda sekali-pakai
    // (§5.5) tetap memberi 409 pada yang kalah.
    let mut attempt = 1;
    let result = loop {
        let result = run_transaction(
            state,
            audit,
            &log,
            entity_type,
            model,
            company_id,
            user_id,
            &before,
        )
        .await;
        match result {
            Err(ref e) if attempt < RETRY_ATTEMPTS && is_serialization_conflict(e) => {
                let delay = retry_delay(attempt);
                tracing::warn!(
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    "audit restore: retry conflict"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            other => break other,
        }
    };

    // Padanan `finally { discardAudit() }`: tidak ada entri buffer yang boleh tersisa.
    audit.discard();

    let restored = result?;
    // Sesudah commit: status karyawan berubah, dan cache pencabutan token memegang status
    // lamanya (mis. DELETED) selama 60 detik.
    if entity_type == EntityType::Employee {
        state.invalidate_token_revocation(log.entity_id);
    }
    Ok(restored)
}

const RETRY_ATTEMPTS: u32 = 3;
const RETRY_BASE_MS: u64 = 150;

/// SQLSTATE 40001 (serialization_failure) / 40P01 (deadlock_detected) — padanan P2034.
pub(crate) fn is_serialization_conflict(err: &ApiError) -> bool {
    let ApiError::Internal(e) = err else {
        return false;
    };
    let Some(sqlx::Error::Database(db)) = e.downcast_ref::<sqlx::Error>() else {
        return false;
    };
    matches!(db.code().as_deref(), Some("40001" | "40P01"))
}

fn retry_delay(attempt: u32) -> std::time::Duration {
    let jitter = u64::from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or_default(),
    ) % RETRY_BASE_MS;
    std::time::Duration::from_millis(RETRY_BASE_MS * (1 << (attempt - 1)) + jitter)
}

#[allow(clippy::too_many_arguments)]
async fn run_transaction(
    state: &AppState,
    audit: &AuditHandle,
    log: &LogRow,
    entity_type: EntityType,
    model: Model,
    company_id: i32,
    user_id: i32,
    before: &Value,
) -> ApiResult<Value> {
    let mut tx = state.db.begin().await?;
    if is_serializable(entity_type) {
        sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *tx)
            .await?;
    }

    let outcome = apply_restore(&mut tx, log, entity_type, company_id, user_id, before).await?;

    let restored_state = project_row(model, Some(&outcome.row)).map(|r| r.to_json());
    // §5.6 — `before` baris `restore` = keadaan terhapusnya = `after` baris `delete`-nya.
    let deleted_state = log.after.clone().or_else(|| log.before.clone());

    let entry = restore_entry(
        log,
        entity_type,
        company_id,
        deleted_state,
        restored_state.clone(),
    );
    // Selalu ditulis (juga bila `AUDIT_LOG_ENABLED=false`, seperti `recordWithin` Node: saklar
    // itu hanya mematikan middleware).
    let ctx = audit.write_context();
    // Galat database di sini diteruskan (bukan ditelan) supaya 40001 sampai ke loop retry.
    let rows = sink::build_rows(
        &mut tx,
        std::slice::from_ref(&entry),
        &ctx,
        sink::Lookup::InTransaction,
    )
    .await?;
    let restore_log_uuid = sink::insert_rows(&mut tx, &rows).await?.into_iter().next();

    let restored_at_ms = Utc::now().timestamp_millis();
    // §5.5 — SELURUH baris `delete` entitas ini yang belum ter-restore (baris kembar dari
    // transaksi yang di-retry ikut ditandai).
    let marked = sqlx::query(
        r#"UPDATE "auditLog"
              SET "restoredAt" = $1, "restoredById" = $2, "restoredByLogId" = $3::uuid
            WHERE "companyId" = $4 AND "entityType" = $5::text::"AuditEntityType"
              AND "entityId" = $6 AND action = 'delete'::"AuditAction" AND "restoredAt" IS NULL"#,
    )
    .bind(naive_from_millis(restored_at_ms))
    .bind(user_id)
    .bind(restore_log_uuid.clone())
    .bind(company_id)
    .bind(&log.entity_type)
    .bind(log.entity_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if marked == 0 {
        // Balapan: restore lain menandainya lebih dulu. Melempar di dalam transaksi
        // membatalkan `UPDATE` entitas yang sudah ditulis di atas.
        return Err(already_restored(log));
    }

    tx.commit().await?;

    Ok(json!({
        "id": log.uuid,
        "entityType": log.entity_type,
        "entity": { "id": log.entity_uuid, "label": log.entity_label },
        "restoreLogId": restore_log_uuid,
        "restoredAt": restored_at_ms,
        "after": restored_state.unwrap_or(Value::Null),
        "warnings": outcome.warnings,
        "notRestored": outcome.not_restored.iter().map(NotRestored::json).collect::<Vec<_>>(),
    }))
}

async fn apply_restore(
    conn: &mut PgConnection,
    log: &LogRow,
    entity_type: EntityType,
    company_id: i32,
    user_id: i32,
    before: &Value,
) -> ApiResult<Outcome> {
    match entity_type {
        EntityType::Company => restore_company(conn, log, before, company_id).await,
        EntityType::Employee => restore_employee(conn, log, before, company_id, user_id).await,
        EntityType::Store => restore_store(conn, log, before, company_id).await,
        EntityType::StoreGroup => restore_read_store_group(conn, log, company_id).await,
        EntityType::StoreVisitForm => restore_form(conn, log, before, company_id).await,
        EntityType::StoreVisitPlan => restore_plan(conn, log, company_id).await,
        EntityType::Geofence => {
            restore_flag(conn, log, Model::Geofence, "geofence", company_id).await
        }
        EntityType::NegativeZone => {
            restore_flag(conn, log, Model::NegativeZone, "negativeZone", company_id).await
        }
        EntityType::Customer => restore_customer(conn, log, before, company_id).await,
        EntityType::Product => restore_product(conn, log, before, company_id).await,
        _ => Err(target_missing(&log.entity_type)),
    }
}

/// Baris sesudah `UPDATE` (semua kolom, seperti hasil `update` Prisma tanpa `select`).
async fn read_after(
    conn: &mut PgConnection,
    model: Model,
    id: i32,
    cols: Columns<'_>,
    entity_type: &str,
) -> ApiResult<Row> {
    read_rows(&mut *conn, model, cols, &Lookup::Id(id), None, Some(1))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| target_missing(entity_type))
}

/// §0.16 `StoreGroupsService.restore`.
async fn restore_store_group(state: &AppState, company_id: i32, uuid: &str) -> ApiResult<()> {
    let id: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "storeGroup" WHERE uuid = $1::uuid AND "companyId" = $2 LIMIT 1"#,
    )
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(id) = id else {
        return Err(ApiError::coded(
            StatusCode::NOT_FOUND,
            "STORE_GROUP_NOT_FOUND",
            "Store group not found",
        ));
    };
    sqlx::query(r#"UPDATE "storeGroup" SET "isActive" = true, "updatedAt" = $2 WHERE id = $1"#)
        .bind(id)
        .bind(mz_core::time::now_naive())
        .execute(&state.db)
        .await?;
    Ok(())
}

async fn restore_read_store_group(
    conn: &mut PgConnection,
    log: &LogRow,
    company_id: i32,
) -> ApiResult<Outcome> {
    let entity_id = log.entity_id;
    let row = read_rows(
        &mut *conn,
        Model::StoreGroup,
        Columns::All,
        &Lookup::filter(move |qb| {
            qb.push(r#"id = "#)
                .push_bind(entity_id)
                .push(r#" AND "companyId" = "#)
                .push_bind(company_id);
        }),
        None,
        Some(1),
    )
    .await?
    .into_iter()
    .next()
    .ok_or_else(|| target_missing(&log.entity_type))?;
    Ok(Outcome::empty(row))
}

/// §5.2 — `status ← before.status`; hanya admin company itu sendiri (§0.12).
async fn restore_company(
    conn: &mut PgConnection,
    log: &LogRow,
    before: &Value,
    company_id: i32,
) -> ApiResult<Outcome> {
    let status = number_from(before, "status")?;
    if log.entity_id != company_id {
        return Err(target_missing(&log.entity_type));
    }
    let exists: Option<i32> = sqlx::query_scalar("SELECT id FROM company WHERE id = $1")
        .bind(log.entity_id)
        .fetch_optional(&mut *conn)
        .await?;
    if exists.is_none() {
        return Err(target_missing(&log.entity_type));
    }
    sqlx::query(r#"UPDATE company SET status = $2, "updatedAt" = $3 WHERE id = $1"#)
        .bind(log.entity_id)
        .bind(status)
        .bind(mz_core::time::now_naive())
        .execute(&mut *conn)
        .await?;
    let row = read_after(
        conn,
        Model::Company,
        log.entity_id,
        Columns::All,
        &log.entity_type,
    )
    .await?;
    Ok(Outcome::empty(row))
}

/// §5.2 — `status ← before.status`; `409 SEAT_LIMIT_REACHED` bila kuota `maxUsers` penuh.
async fn restore_employee(
    conn: &mut PgConnection,
    log: &LogRow,
    before: &Value,
    company_id: i32,
    actor_user_id: i32,
) -> ApiResult<Outcome> {
    let status = number_from(before, "status")?;

    let max_users: Option<i32> =
        sqlx::query_scalar(r#"SELECT "maxUsers" FROM company WHERE id = $1"#)
            .bind(company_id)
            .fetch_optional(&mut *conn)
            .await?;
    if let Some(max_users) = max_users.filter(|m| *m > 0) {
        let memberships: Vec<i32> = sqlx::query_scalar(
            r#"SELECT "userId" FROM "userCompany" WHERE "companyId" = $1 AND status = 1"#,
        )
        .bind(company_id)
        .fetch_all(&mut *conn)
        .await?;
        let other_ids: Vec<i32> = memberships
            .into_iter()
            .filter(|id| *id != log.entity_id)
            .collect();
        let used: i64 = if other_ids.is_empty() {
            0
        } else {
            sqlx::query_scalar("SELECT count(*) FROM users WHERE id = ANY($1) AND status <> $2")
                .bind(other_ids)
                .bind(USER_STATUS_DELETED)
                .fetch_one(&mut *conn)
                .await?
        };
        if used >= i64::from(max_users) {
            return Err(conflict(
                "SEAT_LIMIT_REACHED",
                "This company has no seat left for the restored employee",
                Some(json!({ "maxUsers": max_users, "used": used })),
            ));
        }
    }

    let current: Option<(i32, i32)> = sqlx::query_as("SELECT id, status FROM users WHERE id = $1")
        .bind(log.entity_id)
        .fetch_optional(&mut *conn)
        .await?;
    let Some((_, current_status)) = current else {
        return Err(target_missing(&log.entity_type));
    };

    // `users.status` global: tanpa ini admin company A bisa mengunci akun yang juga aktif di
    // company B lewat restore.
    if status_change_is_blocked(actor_user_id, log.entity_id, current_status, status) {
        mz_core::company::reject_if_shared_account(&mut *conn, log.entity_id, company_id).await?;
    }

    sqlx::query(r#"UPDATE users SET status = $2, "updatedAt" = $3 WHERE id = $1"#)
        .bind(log.entity_id)
        .bind(status)
        .bind(mz_core::time::now_naive())
        .execute(&mut *conn)
        .await?;
    // `select` sempit (id, uuid, username, name, status): baris utuh membawa `password`.
    let row = read_after(
        conn,
        Model::Users,
        log.entity_id,
        Columns::Only(&["id", "uuid", "username", "name", "status"]),
        &log.entity_type,
    )
    .await?;

    // Snapshot bisa memuat DISABLED/DELETED: token lama tidak boleh hidup lagi.
    if status == USER_STATUS_DISABLED || status == USER_STATUS_DELETED {
        mz_core::jwt::revoke_all_user_tokens(&mut *conn, log.entity_id).await?;
    }

    // §5.3 — keanggotaan & peran tidak otomatis kembali.
    let membership: i64 = sqlx::query_scalar(
        r#"SELECT count(*) FROM "userCompany" WHERE "userId" = $1 AND "companyId" = $2 AND status = 1"#,
    )
    .bind(log.entity_id)
    .bind(company_id)
    .fetch_one(&mut *conn)
    .await?;
    let access: i64 = sqlx::query_scalar(
        r#"SELECT count(*) FROM "userAccessLevel" WHERE "userId" = $1 AND "companyId" = $2 AND status = 1"#,
    )
    .bind(log.entity_id)
    .bind(company_id)
    .fetch_one(&mut *conn)
    .await?;
    let mut not_restored = Vec::new();
    if membership == 0 {
        not_restored.push(NotRestored {
            what: "companyMembership",
            reason: "This employee's company membership is not active",
            endpoint: USER_PATCH,
        });
    }
    if access == 0 {
        not_restored.push(NotRestored {
            what: "accessLevel",
            reason: "This employee has no active role in the company",
            endpoint: USER_PATCH,
        });
    }
    Ok(Outcome {
        row,
        warnings: Vec::new(),
        not_restored,
    })
}

fn store_code_taken(code: &str) -> ApiError {
    conflict(
        "STORE_CODE_TAKEN",
        format!("Store code {code} is already used by an active store in this company"),
        Some(json!({ "code": code })),
    )
}

/// §5.2 — `deletedAt = null`, `isActive ← before.isActive` (bukan `true`).
async fn restore_store(
    conn: &mut PgConnection,
    log: &LogRow,
    before: &Value,
    company_id: i32,
) -> ApiResult<Outcome> {
    let is_active = bool_from(before, "isActive")?;

    let current: Option<(i32, String)> =
        sqlx::query_as(r#"SELECT id, code FROM store WHERE id = $1 AND "companyId" = $2 LIMIT 1"#)
            .bind(log.entity_id)
            .bind(company_id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((id, code)) = current else {
        return Err(target_missing(&log.entity_type));
    };

    // `store_code_unique_per_company` partial: diperiksa lebih dulu supaya pesannya menyebut
    // kodenya; unique violation di bawah dipetakan ke kode yang sama (§5.4).
    let clash: Option<String> = sqlx::query_scalar(
        r#"SELECT uuid::text FROM store
            WHERE "companyId" = $1 AND code = $2 AND "deletedAt" IS NULL AND id <> $3 LIMIT 1"#,
    )
    .bind(company_id)
    .bind(&code)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    if clash.is_some() {
        return Err(store_code_taken(&code));
    }

    let updated = sqlx::query(
        r#"UPDATE store SET "deletedAt" = NULL, "isActive" = $2, "updatedAt" = $3 WHERE id = $1"#,
    )
    .bind(id)
    .bind(is_active)
    .bind(mz_core::time::now_naive())
    .execute(&mut *conn)
    .await;
    if let Err(e) = updated {
        if mz_core::db::is_unique_violation(&e) {
            return Err(store_code_taken(&code));
        }
        return Err(e.into());
    }
    let row = read_after(conn, Model::Store, id, Columns::All, &log.entity_type).await?;

    let mut not_restored = Vec::new();
    // §5.3 — region bisa saja sudah diarsip sejak tokonya dihapus.
    if let Some(group_id) = row.int("groupId").and_then(|g| i32::try_from(g).ok()) {
        let active: Option<bool> = sqlx::query_scalar(
            r#"SELECT "isActive" FROM "storeGroup" WHERE id = $1 AND "companyId" = $2 LIMIT 1"#,
        )
        .bind(group_id)
        .bind(company_id)
        .fetch_optional(&mut *conn)
        .await?;
        if active == Some(false) {
            not_restored.push(NotRestored {
                what: "storeGroup",
                reason: "This store's region has been archived",
                endpoint: STORE_PATCH,
            });
        }
    }
    // customers.md §5.4/§8.4 — customer-nya bisa sudah dihapus/diarsip; toko tetap pulih.
    if let Some(customer_id) = row.int("customerId").and_then(|c| i32::try_from(c).ok()) {
        let customer: Option<(bool, bool)> = sqlx::query_as(
            r#"SELECT "deletedAt" IS NOT NULL, "isActive" FROM customer
                WHERE id = $1 AND "companyId" = $2 LIMIT 1"#,
        )
        .bind(customer_id)
        .bind(company_id)
        .fetch_optional(&mut *conn)
        .await?;
        if customer.is_none_or(|(deleted, active)| deleted || !active) {
            not_restored.push(NotRestored {
                what: "customer",
                reason: "This store's customer has been deleted",
                endpoint: STORE_PATCH,
            });
        }
    }
    Ok(Outcome {
        row,
        warnings: Vec::new(),
        not_restored,
    })
}

/// §5.2 — `deletedAt = null`, `isActive ← before.isActive`. Tanpa unique pada `name`.
async fn restore_form(
    conn: &mut PgConnection,
    log: &LogRow,
    before: &Value,
    company_id: i32,
) -> ApiResult<Outcome> {
    let is_active = bool_from(before, "isActive")?;
    let current: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "storeVisitFormTemplate" WHERE id = $1 AND "companyId" = $2 LIMIT 1"#,
    )
    .bind(log.entity_id)
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(id) = current else {
        return Err(target_missing(&log.entity_type));
    };
    sqlx::query(
        r#"UPDATE "storeVisitFormTemplate"
              SET "deletedAt" = NULL, "isActive" = $2, "updatedAt" = $3 WHERE id = $1"#,
    )
    .bind(id)
    .bind(is_active)
    .bind(mz_core::time::now_naive())
    .execute(&mut *conn)
    .await?;
    let row = read_after(
        conn,
        Model::StoreVisitFormTemplate,
        id,
        Columns::All,
        &log.entity_type,
    )
    .await?;
    Ok(Outcome::empty(row))
}

fn plan_exists() -> ApiError {
    conflict(
        "PLAN_ALREADY_EXISTS",
        "Another plan already occupies this sales, date, and store slot",
        None,
    )
}

/// §5.2 — `deletedAt = null`, tiga konflik dan satu peringatan.
async fn restore_plan(
    conn: &mut PgConnection,
    log: &LogRow,
    company_id: i32,
) -> ApiResult<Outcome> {
    let plan: Option<(i32, i32, i32, NaiveDate, bool)> = sqlx::query_as(
        r#"SELECT p.id, p."userId", p."storeId", p."visitDate", s."deletedAt" IS NOT NULL
             FROM "storeVisitPlan" p JOIN store s ON s.id = p."storeId"
            WHERE p.id = $1 AND p."companyId" = $2 LIMIT 1"#,
    )
    .bind(log.entity_id)
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((id, user_id, store_id, visit_date, store_deleted)) = plan else {
        return Err(target_missing(&log.entity_type));
    };

    if store_deleted {
        return Err(unprocessable(
            "STORE_UNAVAILABLE",
            "The store of this plan has been deleted",
        ));
    }

    let sales: Option<i32> = sqlx::query_scalar("SELECT status FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(&mut *conn)
        .await?;
    if sales.is_none_or(|s| s == USER_STATUS_DELETED) {
        return Err(unprocessable(
            "EMPLOYEE_UNAVAILABLE",
            "The sales user of this plan has been deleted",
        ));
    }

    let clash: Option<String> = sqlx::query_scalar(
        r#"SELECT uuid::text FROM "storeVisitPlan"
            WHERE "userId" = $1 AND "visitDate" = $2 AND "storeId" = $3 AND "deletedAt" IS NULL LIMIT 1"#,
    )
    .bind(user_id)
    .bind(visit_date)
    .bind(store_id)
    .fetch_optional(&mut *conn)
    .await?;
    if clash.is_some() {
        return Err(plan_exists());
    }

    let updated = sqlx::query(
        r#"UPDATE "storeVisitPlan" SET "deletedAt" = NULL, "updatedAt" = $2 WHERE id = $1"#,
    )
    .bind(id)
    .bind(mz_core::time::now_naive())
    .execute(&mut *conn)
    .await;
    if let Err(e) = updated {
        if mz_core::db::is_unique_violation(&e) {
            return Err(plan_exists());
        }
        return Err(e.into());
    }
    let row = read_after(
        conn,
        Model::StoreVisitPlan,
        id,
        Columns::All,
        &log.entity_type,
    )
    .await?;

    let warnings = past_plan_warning(visit_date, Utc::now().date_naive())
        .into_iter()
        .collect();
    Ok(Outcome {
        row,
        warnings,
        not_restored: Vec::new(),
    })
}

/// §5.2, §9.5 — `geofence`/`negativeZone`: `isActive = true`, tanpa konflik.
async fn restore_flag(
    conn: &mut PgConnection,
    log: &LogRow,
    model: Model,
    table: &str,
    company_id: i32,
) -> ApiResult<Outcome> {
    let current: Option<i32> = sqlx::query_scalar(&format!(
        r#"SELECT id FROM "{table}" WHERE id = $1 AND "companyId" = $2 LIMIT 1"#
    ))
    .bind(log.entity_id)
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(id) = current else {
        return Err(target_missing(&log.entity_type));
    };
    sqlx::query(&format!(
        r#"UPDATE "{table}" SET "isActive" = true, "updatedAt" = $2 WHERE id = $1"#
    ))
    .bind(id)
    .bind(mz_core::time::now_naive())
    .execute(&mut *conn)
    .await?;
    let row = read_after(conn, model, id, Columns::All, &log.entity_type).await?;
    Ok(Outcome::empty(row))
}

fn customer_code_taken(code: &str) -> ApiError {
    conflict(
        "CUSTOMER_CODE_TAKEN",
        format!("Customer code {code} is already used in this company"),
        None,
    )
}

/// customers.md §8.4 — kembaran `restore_store`.
async fn restore_customer(
    conn: &mut PgConnection,
    log: &LogRow,
    before: &Value,
    company_id: i32,
) -> ApiResult<Outcome> {
    let is_active = bool_from(before, "isActive")?;
    let current: Option<(i32, String)> = sqlx::query_as(
        r#"SELECT id, code FROM customer WHERE id = $1 AND "companyId" = $2 LIMIT 1"#,
    )
    .bind(log.entity_id)
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((id, code)) = current else {
        return Err(target_missing(&log.entity_type));
    };

    // `findFirst({ companyId, code, deletedAt: null })` lalu `clash.id !== current.id`.
    let clash: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM customer WHERE "companyId" = $1 AND code = $2 AND "deletedAt" IS NULL LIMIT 1"#,
    )
    .bind(company_id)
    .bind(&code)
    .fetch_optional(&mut *conn)
    .await?;
    if clash.is_some_and(|c| c != id) {
        return Err(customer_code_taken(&code));
    }

    let updated = sqlx::query(
        r#"UPDATE customer SET "deletedAt" = NULL, "isActive" = $2, "updatedAt" = $3 WHERE id = $1"#,
    )
    .bind(id)
    .bind(is_active)
    .bind(mz_core::time::now_naive())
    .execute(&mut *conn)
    .await;
    if let Err(e) = updated {
        if mz_core::db::is_unique_violation(&e) {
            return Err(customer_code_taken(&code));
        }
        return Err(e.into());
    }
    let row = read_after(conn, Model::Customer, id, Columns::All, &log.entity_type).await?;
    Ok(Outcome::empty(row))
}

fn product_sku_taken(sku: &str) -> ApiError {
    conflict(
        "PRODUCT_SKU_TAKEN",
        format!("SKU {sku} is already used by an active product in this company"),
        None,
    )
}

/// product_master.md §0.35/§7.4 — kembaran `restore_customer`; harga tidak ikut pulih.
async fn restore_product(
    conn: &mut PgConnection,
    log: &LogRow,
    before: &Value,
    company_id: i32,
) -> ApiResult<Outcome> {
    let is_active = bool_from(before, "isActive")?;
    let current: Option<(i32, String)> =
        sqlx::query_as(r#"SELECT id, sku FROM product WHERE id = $1 AND "companyId" = $2 LIMIT 1"#)
            .bind(log.entity_id)
            .bind(company_id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((id, sku)) = current else {
        return Err(target_missing(&log.entity_type));
    };

    let clash: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM product
            WHERE "companyId" = $1 AND sku = $2 AND "deletedAt" IS NULL AND id <> $3 LIMIT 1"#,
    )
    .bind(company_id)
    .bind(&sku)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    if clash.is_some() {
        return Err(product_sku_taken(&sku));
    }

    let updated = sqlx::query(
        r#"UPDATE product SET "deletedAt" = NULL, "isActive" = $2, "updatedAt" = $3 WHERE id = $1"#,
    )
    .bind(id)
    .bind(is_active)
    .bind(mz_core::time::now_naive())
    .execute(&mut *conn)
    .await;
    if let Err(e) = updated {
        if mz_core::db::is_unique_violation(&e) {
            return Err(product_sku_taken(&sku));
        }
        return Err(e.into());
    }
    let row = read_after(conn, Model::Product, id, Columns::All, &log.entity_type).await?;

    let mut not_restored = Vec::new();
    let still_deleted: i64 = sqlx::query_scalar(
        r#"SELECT count(*) FROM "priceListItem" WHERE "productId" = $1 AND "deletedAt" IS NOT NULL"#,
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    if still_deleted > 0 {
        not_restored.push(NotRestored {
            what: "priceListItem",
            reason: "This product's prices were removed when it was deleted and are not restored automatically",
            endpoint: "PUT /v1/admin/price-lists/:id/items",
        });
    }
    Ok(Outcome {
        row,
        warnings: Vec::new(),
        not_restored,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log() -> LogRow {
        LogRow {
            uuid: "u".into(),
            entity_type: "store".into(),
            entity_id: 7,
            entity_uuid: Some("eu".into()),
            entity_label: Some("JOG01".into()),
            sub_entity: None,
            action: "delete".into(),
            actor_type: "user".into(),
            actor_id: Some(1),
            actor_name: None,
            actor_role: None,
            before: Some(json!({"isActive": true, "deletedAt": null})),
            after: Some(json!({"isActive": true, "deletedAt": "2026-01-01T00:00:00.000Z"})),
            changed_fields: vec!["deletedAt".into()],
            is_restorable: true,
            restored_at: None,
            restored_by_log_id: None,
            created_at: NaiveDate::from_ymd_opt(2026, 1, 1)
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .unwrap(),
        }
    }

    fn body(e: ApiError) -> (u16, Value) {
        match e {
            ApiError::Body { status, body } => (status.as_u16(), body),
            other => panic!("bukan Body: {other:?}"),
        }
    }

    #[test]
    fn guards_in_order_and_shapes() {
        let l = log();
        let created = millis(l.created_at);
        assert!(assert_restorable(&l, created + 1000).is_ok());
        // Tepat di batas masih boleh; lewat 1 ms ditolak.
        let edge = created + AUDIT_RETENTION_DAYS * MS_PER_DAY;
        assert!(assert_restorable(&l, edge).is_ok());
        let (s, b) = body(assert_restorable(&l, edge + 1).unwrap_err());
        assert_eq!(s, 409);
        assert_eq!(b["code"], "RESTORE_WINDOW_EXPIRED");
        assert_eq!(b["message"], "Restore window of 90 days has passed");
        assert_eq!(b["details"]["createdAt"], created);
        assert_eq!(b["details"]["expiresAt"], edge);

        let mut not = log();
        not.is_restorable = false;
        let (_, b) = body(assert_restorable(&not, created).unwrap_err());
        assert_eq!(b["code"], "NOT_RESTORABLE");
        assert_eq!(b["details"], json!({"action": "delete", "subEntity": null}));

        // Sudah dipulihkan menang atas kedaluwarsa dan atas NOT_RESTORABLE.
        let mut done = log();
        done.is_restorable = false;
        done.restored_at = Some(l.created_at + chrono::Duration::days(1));
        done.restored_by_log_id = Some("rl".into());
        let (_, b) = body(assert_restorable(&done, edge + 10).unwrap_err());
        assert_eq!(b["code"], "ALREADY_RESTORED");
        assert_eq!(b["message"], "This deletion has already been restored");
        assert_eq!(b["details"]["restoredByLogId"], "rl");
        assert_eq!(
            b["details"]["restoredAt"],
            millis(done.restored_at.unwrap())
        );
    }

    #[test]
    fn snapshot_readers() {
        let s = json!({"isActive": false, "status": 3, "bad": "x", "big": 99999999999_i64});
        assert!(!bool_from(&s, "isActive").unwrap());
        assert_eq!(number_from(&s, "status").unwrap(), 3);
        for f in ["bad", "missing", "big"] {
            let (_, b) = body(number_from(&s, f).unwrap_err());
            assert_eq!(b["code"], "NOT_RESTORABLE");
            assert_eq!(b["details"]["field"], f);
        }
        let (_, b) = body(bool_from(&s, "status").unwrap_err());
        assert_eq!(
            b["message"],
            "Snapshot is missing \"status\", so this entry cannot be restored"
        );
    }

    #[test]
    fn serializable_only_for_unique_entities() {
        for e in [
            EntityType::Store,
            EntityType::StoreVisitPlan,
            EntityType::Customer,
            EntityType::Product,
            EntityType::PriceList,
        ] {
            assert!(is_serializable(e), "{e}");
        }
        for e in [
            EntityType::Company,
            EntityType::Employee,
            EntityType::StoreGroup,
            EntityType::StoreVisitForm,
            EntityType::Geofence,
            EntityType::NegativeZone,
            EntityType::Role,
        ] {
            assert!(!is_serializable(e), "{e}");
        }
    }

    #[test]
    fn every_entity_type_maps_to_its_model() {
        for e in EntityType::ALL {
            let m = model_of(*e);
            // Nama model = nama entitas, kecuali tiga pengecualian skema.
            let expected = match e {
                EntityType::Employee => "users",
                EntityType::StoreVisitForm => "storeVisitFormTemplate",
                EntityType::Role => "accessLevel",
                other => other.as_str(),
            };
            assert_eq!(m.as_str(), expected);
        }
    }

    #[test]
    fn past_plan_warning_text() {
        let d = |y, m, dd| NaiveDate::from_ymd_opt(y, m, dd).unwrap();
        assert_eq!(past_plan_warning(d(2026, 11, 16), d(2026, 11, 16)), None);
        assert_eq!(past_plan_warning(d(2026, 11, 17), d(2026, 11, 16)), None);
        assert_eq!(
            past_plan_warning(d(2026, 11, 15), d(2026, 11, 16)).as_deref(),
            Some("The 2026-11-15 schedule is in the past; the visit can no longer be carried out")
        );
    }

    #[test]
    fn shared_account_rule_matches_assert_no_shared_account_change() {
        // Diri sendiri atau status tak berubah → tidak pernah diblok.
        assert!(!status_change_is_blocked(5, 5, 0, 1));
        assert!(!status_change_is_blocked(5, 6, 1, 1));
        assert!(status_change_is_blocked(5, 6, 0, 1));
    }

    #[test]
    fn restore_entry_shape() {
        let l = log();
        let e = restore_entry(
            &l,
            EntityType::Store,
            9,
            l.after.clone(),
            Some(json!({"isActive": true, "deletedAt": null})),
        );
        assert_eq!(e.action, Action::Restore);
        assert_eq!(e.company_id, Some(9));
        assert_eq!(e.entity_id, 7);
        assert_eq!(e.entity_uuid.as_deref(), Some("eu"));
        assert_eq!(e.entity_label.as_deref(), Some("JOG01"));
        assert!(e.sub_entity.is_none() && !e.is_restorable);
        assert_eq!(e.changed_fields, vec!["deletedAt".to_owned()]);
        assert_eq!(e.before, l.after);
    }

    #[test]
    fn uuid_required_for_store_group() {
        let mut l = log();
        l.entity_uuid = None;
        let (_, b) = body(entity_uuid_or_throw(&l).unwrap_err());
        assert_eq!(b["code"], "NOT_RESTORABLE");
        assert_eq!(
            b["message"],
            "This audit entry does not point at a restorable row"
        );
    }
}
