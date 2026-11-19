//! Port `OvertimeApprovalsService` (`src/overtime/overtime-approvals.service.ts`): sisi
//! approver pengajuan lembur (specs/004 T046, contract §B). Semua jalur dimulai dengan cek
//! "pemanggil ada di himpunan approver EFEKTIF" (FR-018). Baca tetap jalan saat fitur
//! nonaktif (`readOnly`), tulis ditolak `403 OVERTIME_DISABLED`.
//!
//! Transaksi = `prisma.$transaction` bawaan (READ COMMITTED, tanpa retry). `overtimeRequest`
//! tidak diaudit Node (tak ada di daftar model audit), jadi tak ada `AuditHandle` di sini;
//! `discardAudit` Node di jalur galat tak berpadanan.

use std::collections::HashMap;

use chrono::{NaiveDate, NaiveDateTime};
use mz_core::company::require_company_id;
use mz_core::enums::rank_of;
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState};
use sqlx::{PgConnection, Postgres, QueryBuilder};

use super::dto::{ApprovalsQuotaQuery, ListApprovalsQuery, MAX_LIMIT, OrderInput};
use super::entity::{
    ApprovalDetail, ApprovalList, ApprovalRow, OrderRefusalCode, OrderResult, PaginationMeta,
    QuotaEntry,
};
use super::rules::{
    ListMode, OrderTarget, OrderUuid, assemble_order_result, assemble_quota_entries,
    deadline_passed, disabled, list_statuses, not_approver, not_found_error, order_day_refusal,
    order_employee_refusal, order_past_date, parse_order_requested_sec, parse_order_uuids,
    range_reversed, resolve_targets, self_decision, state_conflict_error, total_pages,
    validate_order,
};
use crate::calendar::{DayInfo, DayType, day_info_for, load_batch};
use crate::hr::common::clock::company_clock;
use crate::hr::common::constants::{DEFAULT_OVERTIME_ENABLED, DEFAULT_RETRO_WINDOW_DAYS};
use crate::hr::common::decidable::is_decidable;
use crate::hr::common::dto::is_uuid_any;
use crate::hr::common::eligibility::{load_overtime_employee, load_overtime_employees};
use crate::hr::common::enums::{OvertimeRequestKind, OvertimeRequestStatus};
use crate::hr::common::errors::{is_date_taken_violation, map_overtime_unique};
use crate::hr::common::mail::{self, OvertimeDecision, OvertimeMailRecipient};
use crate::hr::common::mapper::{
    OVERTIME_REQUEST_COLUMNS, OVERTIME_REQUEST_FROM, OvertimeLiveDay, OvertimeRequestEntity,
    OvertimeRequestRow, date_key_of, live_day_of, to_overtime_request_entity,
};
use crate::hr::common::quota::{
    QuotaEvaluateInput, QuotaEvaluateManyItem, assert_confirmed, evaluate, evaluate_many,
};
use crate::hr::common::settings_svc::{
    EffectiveApprover, effective_approvers, ensure_settings, read_settings_row,
};
use crate::locks::{lock_employee, lock_settings_for_share};
use crate::math::{QuotaEvaluation, end_of_local_day_instant};
use crate::zone::{ms_of, ndt};

/// Status yang memenuhi partial unique `overtimeRequest_one_active_per_day`.
const ACTIVE_DAY_STATUSES_SQL: &str =
    "('PENDING_APPROVAL', 'PENDING_EMPLOYEE', 'APPROVED', 'COMPLETED')";

/// Company pemanggil + baris pengaturan dibuat malas (owner ter-seed, R6).
async fn resolve_company(state: &AppState, user_id: i32) -> ApiResult<i32> {
    let company_id = require_company_id(&state.db, user_id).await?;
    ensure_settings(&state.db, company_id).await?;
    Ok(company_id)
}

/// `settings?.enabled ?? DEFAULT_OVERTIME_ENABLED`.
fn enabled_of(row: Option<(bool, i32)>) -> bool {
    row.map_or(DEFAULT_OVERTIME_ENABLED, |(enabled, _)| enabled)
}

fn day_type_str(d: DayType) -> &'static str {
    match d {
        DayType::Workday => "WORKDAY",
        DayType::Holiday => "HOLIDAY",
    }
}

fn int32(v: i64, name: &str) -> ApiResult<i32> {
    i32::try_from(v).map_err(|_| ApiError::internal(anyhow::anyhow!("{name} di luar Int: {v}")))
}

/// Baca ulang satu baris lewat `OVERTIME_REQUEST_SELECT`.
async fn fetch_row(conn: &mut PgConnection, id: i32) -> ApiResult<OvertimeRequestRow> {
    Ok(sqlx::query_as::<_, OvertimeRequestRow>(&format!(
        "SELECT {OVERTIME_REQUEST_COLUMNS} FROM {OVERTIME_REQUEST_FROM} WHERE r.id = $1"
    ))
    .bind(id)
    .fetch_one(conn)
    .await?)
}

/// `canDecide` (cerminan `guardDecision`, bukan penggantinya).
fn can_decide(row: &OvertimeRequestRow, user_id: i32, sole: bool, now: NaiveDateTime) -> bool {
    is_decidable(
        row.kind,
        row.status,
        row.user_id,
        row.response_deadline,
        user_id,
        sole,
        now,
    )
}

// ── daftar & riwayat ─────────────────────────────────────────────────────────

fn push_list_where(
    qb: &mut QueryBuilder<'_, Postgres>,
    company_id: i32,
    statuses: Option<&[OvertimeRequestStatus]>,
    q: &ListApprovalsQuery,
) {
    qb.push(r#"r."companyId" = "#).push_bind(company_id);
    if let Some(statuses) = statuses {
        let list: Vec<String> = statuses.iter().map(|s| s.as_str().to_owned()).collect();
        qb.push(" AND r.status = ANY(")
            .push_bind(list)
            .push(r#"::text[]::"OvertimeRequestStatus"[])"#);
    }
    if let Some(uuid) = &q.user_id {
        qb.push(" AND eu.uuid = ")
            .push_bind(uuid.clone())
            .push("::uuid");
    }
    if let Some(from) = q.from {
        qb.push(" AND r.date >= ").push_bind(from);
    }
    if let Some(to) = q.to {
        qb.push(" AND r.date <= ").push_bind(to);
    }
}

/// `listRows(userId, query, mode)`: `list` (Menunggu) dan `history` (riwayat).
pub async fn list_rows(
    state: &AppState,
    user_id: i32,
    q: &ListApprovalsQuery,
    mode: ListMode,
) -> ApiResult<ApprovalList> {
    let company_id = resolve_company(state, user_id).await?;
    let now = now_naive();

    let approvers = effective_approvers(&state.db, company_id).await?;
    let settings = read_settings_row(&state.db, company_id).await?;
    let mut conn = state.db.acquire().await?;
    let clock = company_clock(&mut conn, company_id, now).await?;
    // FR-018: admin yang bukan approver pun tak melihat apa-apa.
    if !approvers.iter().any(|a| a.id == user_id) {
        return Err(not_approver());
    }
    let read_only = !enabled_of(settings);

    let page = q.page;
    let limit = q.limit.min(MAX_LIMIT);
    let statuses = list_statuses(q.status.as_deref(), mode);
    if let (Some(from), Some(to)) = (q.from, q.to)
        && from > to
    {
        return Err(range_reversed());
    }

    let mut count = QueryBuilder::<Postgres>::new(
        r#"SELECT COUNT(*) FROM "overtimeRequest" r JOIN users eu ON eu.id = r."userId" WHERE "#,
    );
    push_list_where(&mut count, company_id, statuses.as_deref(), q);
    let total: i64 = count.build_query_scalar().fetch_one(&mut *conn).await?;

    let mut select = QueryBuilder::<Postgres>::new(format!(
        "SELECT {OVERTIME_REQUEST_COLUMNS} FROM {OVERTIME_REQUEST_FROM} WHERE "
    ));
    push_list_where(&mut select, company_id, statuses.as_deref(), q);
    select.push(match mode {
        ListMode::Pending => r#" ORDER BY r."responseDeadline" ASC, r.date ASC, r.id ASC"#,
        ListMode::History => " ORDER BY r.date DESC, r.id DESC",
    });
    select
        .push(" LIMIT ")
        .push_bind(limit)
        .push(" OFFSET ")
        .push_bind((page - 1) * limit);
    let rows: Vec<OvertimeRequestRow> = select.build_query_as().fetch_all(&mut *conn).await?;

    // US1 skenario 5: kuota SEKARANG, satu batch untuk semua baris halaman ini.
    let items: Vec<QuotaEvaluateManyItem<i32>> = rows
        .iter()
        .map(|r| QuotaEvaluateManyItem {
            key: r.id,
            user_id: r.user_id,
            date: r.date,
            requested_sec: i64::from(r.requested_sec),
            exclude_request_id: Some(r.id),
        })
        .collect();
    let quotas = evaluate_many(
        &mut conn,
        company_id,
        clock.workweek_start_day,
        ms_of(now),
        &items,
    )
    .await?;

    let sole = approvers.len() < 2;
    let mut data = Vec::with_capacity(rows.len());
    for row in &rows {
        let Some(ev) = quotas.get(&row.id) else {
            return Err(ApiError::internal(anyhow::anyhow!(
                "Evaluasi kuota tak ada untuk request {}",
                row.id
            )));
        };
        // Hari "live" baris yang belum disetujui = hasil kalender yang sama dengan kuota.
        let live = OvertimeLiveDay {
            day_type: ev.view.day_type,
            normal_sec: ev.sec.normal_sec,
        };
        data.push(ApprovalRow {
            request: to_overtime_request_entity(row, Some(&live)),
            quota: ev.view.clone(),
            can_decide: mode == ListMode::Pending
                && !read_only
                && can_decide(row, user_id, sole, now),
        });
    }

    Ok(ApprovalList {
        data,
        meta: PaginationMeta {
            total,
            page,
            limit,
            total_pages: total_pages(total, limit),
        },
        read_only,
    })
}

// ── detail ───────────────────────────────────────────────────────────────────

/// `getOne(userId, uuid)`: 404 bila bukan approver efektif atau bukan milik company.
pub async fn get_one(state: &AppState, user_id: i32, uuid: &str) -> ApiResult<ApprovalDetail> {
    let company_id = resolve_company(state, user_id).await?;
    let now = now_naive();
    let approvers = effective_approvers(&state.db, company_id).await?;
    let settings = read_settings_row(&state.db, company_id).await?;
    if !approvers.iter().any(|a| a.id == user_id) {
        return Err(not_found_error());
    }
    if !is_uuid_any(uuid) {
        return Err(not_found_error());
    }

    let mut conn = state.db.acquire().await?;
    let row: Option<OvertimeRequestRow> = sqlx::query_as(&format!(
        r#"SELECT {OVERTIME_REQUEST_COLUMNS} FROM {OVERTIME_REQUEST_FROM}
           WHERE r.uuid = $1::uuid AND r."companyId" = $2"#
    ))
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Err(not_found_error());
    };

    let live = if row.day_type.is_none() {
        let batch = load_batch(&mut conn, company_id, row.date, row.date, &[row.user_id]).await?;
        Some(live_day_of(&day_info_for(
            &batch,
            row.user_id,
            row.date,
            ms_of(now),
        )?))
    } else {
        None
    };

    let read_only = !enabled_of(settings);
    Ok(ApprovalDetail {
        request: to_overtime_request_entity(&row, live.as_ref()),
        can_decide: !read_only && can_decide(&row, user_id, approvers.len() < 2, now),
    })
}

// ── keputusan ────────────────────────────────────────────────────────────────

/// Baris jalur keputusan (`DECIDE_SELECT`): + email pengaju (`users.username`).
#[derive(sqlx::FromRow)]
struct DecideRow {
    #[sqlx(flatten)]
    row: OvertimeRequestRow,
    requested_by_username: String,
}

/// Data email yang dikumpulkan di dalam transaksi, dikirim SETELAH commit.
struct DecidedMail {
    to: OvertimeMailRecipient,
    decider_name: String,
    employee_name: String,
    date_key: String,
    requested_sec: i64,
}

fn mail_of(d: &DecideRow, decider: &EffectiveApprover) -> DecidedMail {
    DecidedMail {
        // Email login disimpan di `users.username`.
        to: OvertimeMailRecipient {
            email: d.requested_by_username.clone(),
            name: d.row.requested_by_name.clone(),
        },
        decider_name: decider.name.clone(),
        employee_name: d.row.user_name.clone(),
        date_key: date_key_of(d.row.date),
        requested_sec: i64::from(d.row.requested_sec),
    }
}

fn send_decided(
    state: &AppState,
    m: &DecidedMail,
    decision: OvertimeDecision,
    reject_reason: Option<&str>,
) {
    mail::decided(
        state,
        &m.to,
        decision,
        &m.decider_name,
        &m.employee_name,
        &m.date_key,
        m.requested_sec,
        reject_reason,
        None,
    );
}

/// `guardDecision`: T5 settings FOR SHARE → T9 himpunan approver dibaca ulang → baris
/// (company-scoped) → SELF + PENDING_APPROVAL → aturan diri sendiri → T10 tenggat.
async fn guard_decision(
    conn: &mut PgConnection,
    company_id: i32,
    user_id: i32,
    uuid: &str,
    now: NaiveDateTime,
) -> ApiResult<(DecideRow, EffectiveApprover)> {
    let settings = lock_settings_for_share(&mut *conn, company_id).await?;
    if !settings.map_or(DEFAULT_OVERTIME_ENABLED, |s| s.enabled) {
        return Err(disabled());
    }

    let approvers = effective_approvers(&mut *conn, company_id).await?;
    let Some(decider) = approvers.iter().find(|a| a.id == user_id).cloned() else {
        return Err(not_approver());
    };

    if !is_uuid_any(uuid) {
        return Err(not_found_error());
    }
    let row: Option<DecideRow> = sqlx::query_as(&format!(
        r#"SELECT {OVERTIME_REQUEST_COLUMNS}, rb.username AS requested_by_username
           FROM {OVERTIME_REQUEST_FROM}
           WHERE r.uuid = $1::uuid AND r."companyId" = $2"#
    ))
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Err(not_found_error());
    };

    if row.row.kind != OvertimeRequestKind::SELF
        || row.row.status != OvertimeRequestStatus::PENDING_APPROVAL
    {
        return Err(state_conflict_error());
    }
    // Skenario 9: pengaju tak memutuskan pengajuannya sendiri kecuali approver tunggal.
    if row.row.user_id == user_id && approvers.len() >= 2 {
        return Err(self_decision());
    }
    // T10: keputusan yang datang setelah tenggat tapi sebelum tick ditolak.
    if now >= row.row.response_deadline {
        return Err(deadline_passed());
    }
    Ok((row, decider))
}

/// `approve(userId, uuid, dto)`: satu transaksi; snapshot hari, kuota dievaluasi ulang
/// (T3), CAS status (T2), hitung di tempat bila tanggal sudah lewat (T062). Email setelah
/// commit.
pub async fn approve(
    state: &AppState,
    user_id: i32,
    uuid: &str,
    confirm_over_quota: Option<bool>,
) -> ApiResult<OvertimeRequestEntity> {
    let company_id = resolve_company(state, user_id).await?;
    let now = now_naive();
    let clock = {
        let mut conn = state.db.acquire().await?;
        company_clock(&mut conn, company_id, now).await?
    };

    let mut tx = state.db.begin().await?;
    let (decide, decider) = guard_decision(&mut tx, company_id, user_id, uuid, now).await?;
    let row = &decide.row;

    // Snapshot hari (R5) dari kalender saat persetujuan.
    let batch = load_batch(&mut tx, company_id, row.date, row.date, &[row.user_id]).await?;
    let info: DayInfo = day_info_for(&batch, row.user_id, row.date, ms_of(now))?;

    // T3: kuota dievaluasi ulang DI DALAM transaksi.
    let evaluation = evaluate(
        &mut tx,
        &QuotaEvaluateInput {
            company_id,
            user_id: row.user_id,
            date: row.date,
            requested_sec: i64::from(row.requested_sec),
            as_of_ms: ms_of(now),
            workweek_start_day: clock.workweek_start_day,
            exclude_request_id: Some(row.id),
        },
    )
    .await?;
    assert_confirmed(&evaluation, confirm_over_quota)?;

    // T2: CAS pada status. Approver lain yang menang duluan → 0 baris → 409.
    let pattern_work_days = i16::try_from(info.pattern_work_days).map_err(|_| {
        ApiError::internal(anyhow::anyhow!(
            "patternWorkDays di luar SmallInt: {}",
            info.pattern_work_days
        ))
    })?;
    let done = sqlx::query(
        r#"UPDATE "overtimeRequest"
           SET status = 'APPROVED', "approvedAt" = $2, "decidedById" = $3, "decidedAt" = $2,
               "dayType" = $4::"OvertimeDayType", "baseNormalSec" = $5, "dailyLimitSec" = $6,
               "patternWorkDays" = $7, "exceedsDailyLimit" = $8, "exceedsLimit" = $8,
               "updatedAt" = $9
           WHERE id = $1 AND status = 'PENDING_APPROVAL'"#,
    )
    .bind(row.id)
    .bind(now)
    .bind(user_id)
    .bind(day_type_str(info.day_type))
    .bind(int32(info.base_normal_sec, "baseNormalSec")?)
    .bind(int32(info.daily_limit_sec, "dailyLimitSec")?)
    .bind(pattern_work_days)
    // Label mingguan baru dihitung setelah komputasi (R7, Fase 5).
    .bind(evaluation.view.exceeds_daily)
    .bind(now_naive())
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        return Err(state_conflict_error());
    }

    // T062 (R10): tanggal sudah lewat → hitung di tempat (→ COMPLETED) di transaksi ini.
    if row.date < clock.local_today {
        crate::compute::compute_one(&mut tx, row.id, now)
            .await
            .map_err(ApiError::internal)?;
    }

    let fresh = fetch_row(&mut tx, row.id).await?;
    let mail = mail_of(&decide, &decider);
    tx.commit().await?;

    send_decided(state, &mail, OvertimeDecision::Approved, None);
    Ok(to_overtime_request_entity(&fresh, None))
}

/// `reject(userId, uuid, dto)`: penjaga sama dengan `approve` minus kuota.
pub async fn reject(
    state: &AppState,
    user_id: i32,
    uuid: &str,
    reason: &str,
) -> ApiResult<OvertimeRequestEntity> {
    if reason.is_empty() {
        // Lapis kedua; DTO sudah menolak dengan 400.
        return Err(ApiError::bad_request("Alasan penolakan wajib diisi."));
    }
    let company_id = resolve_company(state, user_id).await?;
    let now = now_naive();

    let mut tx = state.db.begin().await?;
    let (decide, decider) = guard_decision(&mut tx, company_id, user_id, uuid, now).await?;

    // T2: CAS pada status.
    let done = sqlx::query(
        r#"UPDATE "overtimeRequest"
           SET status = 'REJECTED', "rejectReason" = $2, "decidedById" = $3, "decidedAt" = $4,
               "updatedAt" = $5
           WHERE id = $1 AND status = 'PENDING_APPROVAL'"#,
    )
    .bind(decide.row.id)
    .bind(reason)
    .bind(user_id)
    .bind(now)
    .bind(now_naive())
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        return Err(state_conflict_error());
    }

    let fresh = fetch_row(&mut tx, decide.row.id).await?;
    let mail = mail_of(&decide, &decider);
    tx.commit().await?;

    send_decided(state, &mail, OvertimeDecision::Rejected, Some(reason));
    // Tanpa snapshot: `dayType`/`normalMinutes` null di respons ini.
    Ok(to_overtime_request_entity(&fresh, None))
}

// ── perintah lembur ─────────────────────────────────────────────────────────

/// `resolveOrderTargets`: SELF, uuid bukan anggota company (→ NOT_ELIGIBLE), uuid → id
/// dalam SATU query (status keanggotaan apa pun). Target terurut `users.id` naik.
async fn resolve_order_targets(
    state: &AppState,
    company_id: i32,
    caller: &EffectiveApprover,
    uuids: &[OrderUuid],
) -> ApiResult<(HashMap<String, OrderRefusalCode>, Vec<OrderTarget>)> {
    let caller_key = caller.uuid.to_lowercase();
    let others: Vec<uuid::Uuid> = uuids
        .iter()
        .filter(|u| u.key != caller_key)
        .filter_map(|u| uuid::Uuid::parse_str(&u.key).ok())
        .collect();
    let members: Vec<(i32, String)> = if others.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            r#"SELECT u.id, u.uuid::text FROM users u
               WHERE u.uuid = ANY($1)
                 AND EXISTS (SELECT 1 FROM "userCompany" uc
                             WHERE uc."userId" = u.id AND uc."companyId" = $2)"#,
        )
        .bind(&others)
        .bind(company_id)
        .fetch_all(&state.db)
        .await?
    };
    Ok(resolve_targets(uuids, caller.id, &caller.uuid, &members))
}

/// Hasil satu transaksi karyawan yang berhasil (`OrderCreated`).
struct OrderCreated {
    request_uuid: String,
    entity: OvertimeRequestEntity,
    to: OvertimeMailRecipient,
    approver_name: String,
}

/// Konteks bersama transaksi per karyawan.
struct OrderCtx<'a> {
    company_id: i32,
    caller_id: i32,
    date: NaiveDate,
    requested_sec: i64,
    reason: &'a str,
    time_zone: &'a str,
    now: NaiveDateTime,
}

/// Galat di dalam transaksi satu karyawan.
enum OrderTxError {
    /// Penolakan per karyawan (bukan galat panggilan).
    Refusal(OrderRefusalCode),
    /// T5: fitur dinonaktifkan di tengah loop.
    Disabled,
    Api(ApiError),
}

impl From<ApiError> for OrderTxError {
    fn from(e: ApiError) -> Self {
        Self::Api(e)
    }
}

impl From<sqlx::Error> for OrderTxError {
    fn from(e: sqlx::Error) -> Self {
        Self::Api(e.into())
    }
}

impl From<anyhow::Error> for OrderTxError {
    fn from(e: anyhow::Error) -> Self {
        Self::Api(e.into())
    }
}

/// Panggilan dibatalkan di tengah loop; `disabled` = `OVERTIME_DISABLED` (tanpa email).
struct OrderAbort {
    error: ApiError,
    disabled: bool,
}

fn is_date_taken(err: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(db) = err
        && db.is_unique_violation()
    {
        let detail = db
            .try_downcast_ref::<sqlx::postgres::PgDatabaseError>()
            .and_then(|pg| pg.detail());
        return is_date_taken_violation(db.constraint(), detail);
    }
    false
}

async fn order_tx(
    conn: &mut PgConnection,
    target: &OrderTarget,
    ctx: &OrderCtx<'_>,
) -> Result<OrderCreated, OrderTxError> {
    let company_id = ctx.company_id;
    // T5: FOR SHARE vs FOR UPDATE milik disable.
    let settings = lock_settings_for_share(&mut *conn, company_id).await?;
    if !settings.map_or(DEFAULT_OVERTIME_ENABLED, |s| s.enabled) {
        return Err(OrderTxError::Disabled);
    }
    let retro_window_days = settings.map_or(DEFAULT_RETRO_WINDOW_DAYS, |s| s.retro_window_days);

    // T9: approver dicopot di tengah loop → 403 untuk sisa panggilan.
    let approvers = effective_approvers(&mut *conn, company_id).await?;
    let Some(caller) = approvers.iter().find(|a| a.id == ctx.caller_id) else {
        return Err(OrderTxError::Api(not_approver()));
    };

    let employee = load_overtime_employee(&mut *conn, company_id, target.user_id).await?;
    // Rank pemanggil dari role efektif yang dibaca ulang di tx ini (T9).
    if let Some(code) = order_employee_refusal(employee.as_ref(), rank_of(&caller.role)) {
        return Err(OrderTxError::Refusal(code));
    }
    let Some(employee) = employee else {
        return Err(OrderTxError::Refusal(
            OrderRefusalCode::OVERTIME_EMPLOYEE_NOT_ELIGIBLE,
        ));
    };

    // T4: lock karyawan DULU, baru baca cuti.
    lock_employee(&mut *conn, company_id, target.user_id).await?;
    let batch = load_batch(
        &mut *conn,
        company_id,
        ctx.date,
        ctx.date,
        &[target.user_id],
    )
    .await?;
    let day = day_info_for(&batch, target.user_id, ctx.date, ms_of(ctx.now))?;
    if let Some(code) = order_day_refusal(&day, ctx.requested_sec) {
        return Err(OrderTxError::Refusal(code));
    }

    // T1: partial unique `overtimeRequest_one_active_per_day` yang memutuskan.
    // R5: ORDER menunggu sampai akhir tanggal; finalAt = akhir tanggal + jendela.
    let key = date_key_of(ctx.date);
    let deadline = end_of_local_day_instant(&key, 0, Some(ctx.time_zone))
        .map_err(|e| ApiError::internal(anyhow::anyhow!("{e}")))?;
    let final_at =
        end_of_local_day_instant(&key, i64::from(retro_window_days), Some(ctx.time_zone))
            .map_err(|e| ApiError::internal(anyhow::anyhow!("{e}")))?;
    let stamp = now_naive();
    let inserted = sqlx::query_scalar::<_, i32>(
        r#"INSERT INTO "overtimeRequest" ("companyId", "userId", "requestedById", kind, date,
             "requestedSec", reason, status, "retroWindowDays", "responseDeadline", "finalAt",
             "createdAt", "updatedAt")
           VALUES ($1, $2, $3, 'ORDER', $4, $5, $6, 'PENDING_EMPLOYEE', $7, $8, $9, $10, $10)
           RETURNING id"#,
    )
    .bind(company_id)
    .bind(target.user_id)
    .bind(ctx.caller_id)
    .bind(ctx.date)
    .bind(int32(ctx.requested_sec, "requestedSec")?)
    .bind(ctx.reason)
    .bind(retro_window_days)
    .bind(ndt(deadline))
    .bind(ndt(final_at))
    .bind(stamp)
    .fetch_one(&mut *conn)
    .await;
    let id = match inserted {
        Ok(id) => id,
        Err(e) if is_date_taken(&e) => {
            return Err(OrderTxError::Refusal(OrderRefusalCode::OVERTIME_DATE_TAKEN));
        }
        Err(e) => return Err(OrderTxError::Api(map_overtime_unique(e))),
    };
    let row = fetch_row(conn, id).await?;

    Ok(OrderCreated {
        request_uuid: target.request_uuid.clone(),
        entity: to_overtime_request_entity(&row, Some(&live_day_of(&day))),
        // Email login disimpan di `users.username`.
        to: OvertimeMailRecipient {
            email: employee.username,
            name: employee.name,
        },
        approver_name: caller.name.clone(),
    })
}

/// `createOneOrder`: satu karyawan, satu transaksi (T12). `Ok(Err(code))` = refusal.
async fn create_one_order(
    state: &AppState,
    target: &OrderTarget,
    ctx: &OrderCtx<'_>,
) -> Result<Result<OrderCreated, OrderRefusalCode>, OrderAbort> {
    let abort = |error: ApiError| OrderAbort {
        error,
        disabled: false,
    };
    let mut tx = state.db.begin().await.map_err(|e| abort(e.into()))?;
    match order_tx(&mut tx, target, ctx).await {
        Ok(created) => match tx.commit().await {
            Ok(()) => Ok(Ok(created)),
            Err(e) if is_date_taken(&e) => Ok(Err(OrderRefusalCode::OVERTIME_DATE_TAKEN)),
            Err(e) => Err(abort(map_overtime_unique(e))),
        },
        // Transaksi di-rollback saat `tx` dilepas.
        Err(OrderTxError::Refusal(code)) => Ok(Err(code)),
        Err(OrderTxError::Disabled) => Err(OrderAbort {
            error: disabled(),
            disabled: true,
        }),
        Err(OrderTxError::Api(e)) => Err(abort(e)),
    }
}

/// Setelah semua commit, sekali untuk semua karyawan yang dibuat; tak pernah gagal.
fn send_order_received(
    state: &AppState,
    created: &[OrderCreated],
    date: NaiveDate,
    requested_sec: i64,
    reason: &str,
) {
    // Nama dari himpunan approver yang dibaca di tx terakhir yang berhasil.
    let Some(last) = created.last() else {
        return;
    };
    let employees: Vec<OvertimeMailRecipient> = created.iter().map(|c| c.to.clone()).collect();
    mail::order_received(
        state,
        &employees,
        &last.approver_name,
        &date_key_of(date),
        requested_sec,
        reason,
    );
}

/// `createOrders(userId, dto)` (US4): hari ini atau ke depan → `PENDING_EMPLOYEE`. Satu
/// transaksi PER karyawan, berurutan menurut `users.id`.
pub async fn create_orders(
    state: &AppState,
    user_id: i32,
    input: &OrderInput,
) -> ApiResult<OrderResult> {
    let order = validate_order(input)?;

    let company_id = resolve_company(state, user_id).await?;
    let now = now_naive();
    let clock = {
        let mut conn = state.db.acquire().await?;
        company_clock(&mut conn, company_id, now).await?
    };
    // Skenario 2: retro hanya lewat pengajuan karyawan sendiri. Hari ini boleh.
    if order.date < clock.local_today {
        return Err(order_past_date(clock.local_today));
    }

    // Pra-cek di luar transaksi: gagal cepat sebelum apa pun dibuat.
    let approvers = effective_approvers(&state.db, company_id).await?;
    let settings = read_settings_row(&state.db, company_id).await?;
    let Some(caller) = approvers.iter().find(|a| a.id == user_id) else {
        return Err(not_approver());
    };
    if !enabled_of(settings) {
        return Err(disabled());
    }

    let (mut refusals, targets) =
        resolve_order_targets(state, company_id, caller, &order.uuids).await?;

    let ctx = OrderCtx {
        company_id,
        caller_id: user_id,
        date: order.date,
        requested_sec: order.requested_sec,
        reason: &order.reason,
        time_zone: &clock.time_zone,
        now,
    };
    let mut created: Vec<OrderCreated> = Vec::new();
    for target in &targets {
        match create_one_order(state, target, &ctx).await {
            Ok(Ok(c)) => created.push(c),
            Ok(Err(code)) => {
                refusals.insert(target.request_uuid.clone(), code);
            }
            Err(abort) => {
                // Baris yang SUDAH commit sah dan tetap menunggu karyawan → tetap di-email,
                // kecuali fitur dinonaktifkan (disable sudah membatalkannya, tanpa email).
                if !abort.disabled {
                    send_order_received(
                        state,
                        &created,
                        order.date,
                        order.requested_sec,
                        &order.reason,
                    );
                }
                return Err(abort.error);
            }
        }
    }

    let by_uuid: HashMap<String, OvertimeRequestEntity> = created
        .iter()
        .map(|c| (c.request_uuid.clone(), c.entity.clone()))
        .collect();
    let (created_list, refused) = assemble_order_result(&order.uuids, &by_uuid, &refusals);

    send_order_received(
        state,
        &created,
        order.date,
        order.requested_sec,
        &order.reason,
    );
    Ok(OrderResult {
        created: created_list,
        refused,
    })
}

/// `orderQuota(userId, query)` (FX-5.5): pratinjau per karyawan untuk form perintah;
/// aturan penolakan sama dengan `createOrders`. Hanya baca, advisory.
pub async fn order_quota(
    state: &AppState,
    user_id: i32,
    q: &ApprovalsQuotaQuery,
) -> ApiResult<Vec<QuotaEntry>> {
    // Lapis kedua (DTO sudah menolak dengan 400).
    let uuids = parse_order_uuids(Some(&q.user_ids))?;
    let date = q.date;
    let requested_sec = parse_order_requested_sec(Some(q.duration_minutes))?;

    let company_id = resolve_company(state, user_id).await?;
    let now = now_naive();
    let approvers = effective_approvers(&state.db, company_id).await?;
    let settings = read_settings_row(&state.db, company_id).await?;
    let mut conn = state.db.acquire().await?;
    let clock = company_clock(&mut conn, company_id, now).await?;
    let Some(caller) = approvers.iter().find(|a| a.id == user_id) else {
        return Err(not_approver());
    };
    if !enabled_of(settings) {
        return Err(disabled());
    }
    if date < clock.local_today {
        return Err(order_past_date(clock.local_today));
    }
    let caller_rank = rank_of(&caller.role);

    let (mut refusals, targets) = resolve_order_targets(state, company_id, caller, &uuids).await?;

    let mut quotas: HashMap<String, QuotaEvaluation> = HashMap::new();
    if !targets.is_empty() {
        let ids: Vec<i32> = targets.iter().map(|t| t.user_id).collect();
        // Aturan yang sama dengan `createOrders` (`overtimeEmployeeWhere`).
        let employees = load_overtime_employees(&mut *conn, company_id, &ids).await?;
        let taken: Vec<i32> = sqlx::query_scalar(&format!(
            r#"SELECT "userId" FROM "overtimeRequest"
               WHERE "companyId" = $1 AND "userId" = ANY($2) AND date = $3
                 AND status IN {ACTIVE_DAY_STATUSES_SQL}"#
        ))
        .bind(company_id)
        .bind(&ids)
        .bind(date)
        .fetch_all(&mut *conn)
        .await?;
        let batch = load_batch(&mut conn, company_id, date, date, &ids).await?;

        let mut evaluable: Vec<&OrderTarget> = Vec::new();
        for target in &targets {
            let employee = employees.iter().find(|e| e.id == target.user_id);
            let code = match order_employee_refusal(employee, caller_rank) {
                Some(code) => Some(code),
                None => {
                    let day = day_info_for(&batch, target.user_id, date, ms_of(now))?;
                    order_day_refusal(&day, requested_sec).or_else(|| {
                        taken
                            .contains(&target.user_id)
                            .then_some(OrderRefusalCode::OVERTIME_DATE_TAKEN)
                    })
                }
            };
            match code {
                Some(code) => {
                    refusals.insert(target.request_uuid.clone(), code);
                }
                None => evaluable.push(target),
            }
        }

        let items: Vec<QuotaEvaluateManyItem<String>> = evaluable
            .iter()
            .map(|t| QuotaEvaluateManyItem {
                key: t.request_uuid.clone(),
                user_id: t.user_id,
                date,
                requested_sec,
                exclude_request_id: None,
            })
            .collect();
        let evaluations = evaluate_many(
            &mut conn,
            company_id,
            clock.workweek_start_day,
            ms_of(now),
            &items,
        )
        .await?;
        quotas = evaluations
            .into_iter()
            .map(|(k, ev)| (k, ev.view))
            .collect();
    }

    assemble_quota_entries(&uuids, &quotas, &refusals)
}
