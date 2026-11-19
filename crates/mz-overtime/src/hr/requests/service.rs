//! Port `src/overtime/overtime-requests.service.ts` (specs/004 T045, contract §A): sisi
//! karyawan pengajuan lembur — kuota, ajukan, daftar milik sendiri, detail, batal, serta
//! terima/tolak perintah lembur (ORDER). Semua route menolak `403 OVERTIME_DISABLED` saat
//! fitur nonaktif, termasuk baca (FR-036).
//!
//! Transaksi = `prisma.$transaction(async tx => …)` bawaan (READ COMMITTED, tanpa retry);
//! lock dan CAS sama dengan Node (T1–T5, T10, T121). Email dikirim sesudah commit,
//! fire-and-forget (`common::mail`). `overtimeRequest` tidak diaudit Node, dan tak ada
//! model ber-audit yang ditulis di sini (`ensure_settings` = statement mentah tanpa audit).

use axum::http::StatusCode;
use chrono::{Duration, NaiveDate, NaiveDateTime};
use mz_core::company::require_company_id;
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState};
use serde::Serialize;
use serde_json::json;
use sqlx::{PgConnection, Postgres, QueryBuilder};

use super::dto::{CreateOvertimeRequestInput, ListMineQuery, QuotaQuery, REASON_MAX_LENGTH};
use crate::calendar::{DayInfo, DayType, LeaveCoverage, day_info_for, load_batch};
use crate::hr::common::clock::company_clock;
use crate::hr::common::constants::{DEFAULT_OVERTIME_ENABLED, DEFAULT_RETRO_WINDOW_DAYS};
use crate::hr::common::dto::is_uuid_any;
use crate::hr::common::eligibility::{is_eligible_overtime_employee, load_overtime_employee};
use crate::hr::common::enums::{OvertimeRequestKind, OvertimeRequestStatus};
use crate::hr::common::errors::{map_overtime_unique, overtime_error};
use crate::hr::common::mail::{self, OvertimeDecision, OvertimeMailRecipient};
use crate::hr::common::mapper::{
    OVERTIME_REQUEST_COLUMNS, OVERTIME_REQUEST_FROM, OvertimeRequestEntity, OvertimeRequestRow,
    date_key_of, live_day_of, normal_sec_live, to_overtime_request_entity,
};
use crate::hr::common::quota::{QuotaEvaluateInput, assert_confirmed, evaluate};
use crate::hr::common::settings_svc::{
    EffectiveApprover, effective_approvers, ensure_settings, read_settings_row,
};
use crate::locks::{lock_employee, lock_settings_for_share};
use crate::math::{
    QuotaEvaluation, duration_exceeds_day_message, end_of_local_day_instant, exceeds_day,
};
use crate::zone::{date_key, ms_of, ndt};

// ── galat ────────────────────────────────────────────────────────────────────

pub use crate::hr::common::errors::{not_found_error, state_conflict_error};

/// `403 OVERTIME_DISABLED`.
pub fn disabled_error() -> ApiError {
    overtime_error(
        StatusCode::FORBIDDEN,
        "OVERTIME_DISABLED",
        "Fitur lembur sedang dinonaktifkan untuk perusahaan ini.",
        None,
    )
}

/// `403 OVERTIME_NOT_REQUESTER`.
pub fn not_requester_error() -> ApiError {
    overtime_error(
        StatusCode::FORBIDDEN,
        "OVERTIME_NOT_REQUESTER",
        "Hanya pengaju yang dapat membatalkan pengajuan lembur ini.",
        None,
    )
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError::internal(anyhow::anyhow!("{e}"))
}

// ── respons ──────────────────────────────────────────────────────────────────

/// `{ data }`.
#[derive(Debug, Serialize)]
pub struct DataResponse<T: Serialize> {
    pub data: T,
}

/// `OvertimePaginationMetaEntity` (urutan kunci = Node).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaginationMeta {
    pub total: i64,
    pub page: i64,
    pub limit: i64,
    pub total_pages: i64,
}

/// `OvertimeRequestListResult`.
#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub data: Vec<OvertimeRequestEntity>,
    pub meta: PaginationMeta,
}

/// `Math.ceil(total / limit)`.
pub fn total_pages(total: i64, limit: i64) -> i64 {
    if limit <= 0 {
        return 0;
    }
    (total + limit - 1).div_euclid(limit)
}

// ── pembantu murni ──────────────────────────────────────────────────────────

/// `addDaysToKey(localToday, -retroWindowDays)`: tanggal paling awal yang boleh diajukan.
pub fn earliest_date(local_today: NaiveDate, retro_window_days: i32) -> NaiveDate {
    local_today - Duration::days(i64::from(retro_window_days))
}

/// `OVERTIME_RETRO_WINDOW_PASSED` bila `date < earliest` (perbandingan kunci `YYYY-MM-DD`).
pub fn retro_window_error(
    date: NaiveDate,
    local_today: NaiveDate,
    retro_window_days: i32,
) -> Option<ApiError> {
    let earliest = earliest_date(local_today, retro_window_days);
    (date < earliest).then(|| {
        overtime_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "OVERTIME_RETRO_WINDOW_PASSED",
            format!(
                "Tanggal ini sudah melewati batas pengajuan mundur ({retro_window_days} hari)."
            ),
            Some(json!({"earliestDate": date_key(earliest), "retroWindowDays": retro_window_days})),
        )
    })
}

/// Penjaga hari (T4 + FX-7.3): cuti sehari penuh → 422; lembur + jam normal setelah cuti
/// setengah hari > 24 jam → 400.
pub fn day_guard(day: &DayInfo, requested_sec: i64) -> ApiResult<()> {
    if day.leave == LeaveCoverage::Full {
        return Err(overtime_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "OVERTIME_EMPLOYEE_ON_LEAVE",
            "Anda cuti sehari penuh di tanggal ini.",
            None,
        ));
    }
    let normal_sec = normal_sec_live(day);
    if exceeds_day(requested_sec, normal_sec) {
        return Err(overtime_error(
            StatusCode::BAD_REQUEST,
            "OVERTIME_DURATION_EXCEEDS_DAY",
            duration_exceeds_day_message(requested_sec, normal_sec),
            None,
        ));
    }
    Ok(())
}

/// Galat `cancel` saat CAS tidak mengubah baris (urutan cek = Node, sesudah 404/403 pihak
/// tak terkait). `None` = CAS berhasil.
pub fn cancel_failure(
    row: &OvertimeRequestRow,
    user_id: i32,
    updated: bool,
    now: NaiveDateTime,
) -> Option<ApiError> {
    if updated {
        return None;
    }
    if row.requested_by_id != user_id {
        return Some(not_requester_error());
    }
    if is_pending(row.status) && row.response_deadline <= now {
        return Some(overtime_error(
            StatusCode::CONFLICT,
            "OVERTIME_DEADLINE_PASSED",
            "Batas waktu pengajuan lembur ini sudah lewat dan tidak dapat dibatalkan lagi.",
            None,
        ));
    }
    Some(overtime_error(
        StatusCode::CONFLICT,
        "OVERTIME_STATE_CONFLICT",
        "Pengajuan lembur ini sudah tidak dapat dibatalkan.",
        Some(json!({"status": row.status.as_str()})),
    ))
}

/// Penjaga status + T10 jalur jawab ORDER (sesudah baris ditemukan).
pub fn order_response_guard(
    status: OvertimeRequestStatus,
    deadline: NaiveDateTime,
    now: NaiveDateTime,
) -> ApiResult<()> {
    if status != OvertimeRequestStatus::PENDING_EMPLOYEE {
        return Err(state_conflict_error());
    }
    if now >= deadline {
        return Err(overtime_error(
            StatusCode::CONFLICT,
            "OVERTIME_DEADLINE_PASSED",
            "Batas waktu menjawab perintah lembur ini sudah lewat.",
            None,
        ));
    }
    Ok(())
}

fn is_pending(status: OvertimeRequestStatus) -> bool {
    matches!(
        status,
        OvertimeRequestStatus::PENDING_APPROVAL | OvertimeRequestStatus::PENDING_EMPLOYEE
    )
}

/// Nilai enum Postgres `OvertimeDayType`.
fn day_type_str(d: DayType) -> &'static str {
    match d {
        DayType::Workday => "WORKDAY",
        DayType::Holiday => "HOLIDAY",
    }
}

// ── pembantu DB ──────────────────────────────────────────────────────────────

/// `assertEnabled(companyId)`: baca tanpa lock; baris kosong = bawaan (aktif).
async fn assert_enabled(state: &AppState, company_id: i32) -> ApiResult<()> {
    let enabled = read_settings_row(&state.db, company_id)
        .await?
        .map_or(DEFAULT_OVERTIME_ENABLED, |(enabled, _)| enabled);
    if enabled {
        Ok(())
    } else {
        Err(disabled_error())
    }
}

/// `lockSettingsForShare` + cek `enabled` (T5). Mengembalikan `retroWindowDays`.
async fn locked_settings(conn: &mut PgConnection, company_id: i32) -> ApiResult<i32> {
    let settings = lock_settings_for_share(conn, company_id).await?;
    if !settings.map_or(DEFAULT_OVERTIME_ENABLED, |s| s.enabled) {
        return Err(disabled_error());
    }
    Ok(settings.map_or(DEFAULT_RETRO_WINDOW_DAYS, |s| s.retro_window_days))
}

/// Satu baris `OVERTIME_REQUEST_SELECT` berdasar `id`.
async fn fetch_by_id(conn: &mut PgConnection, id: i32) -> ApiResult<OvertimeRequestRow> {
    let row = sqlx::query_as::<_, OvertimeRequestRow>(&format!(
        "SELECT {OVERTIME_REQUEST_COLUMNS} FROM {OVERTIME_REQUEST_FROM} WHERE r.id = $1"
    ))
    .bind(id)
    .fetch_one(conn)
    .await?;
    Ok(row)
}

/// `toEntities(companyId, rows, asOf)`: baris tanpa snapshot (`dayType` null) diberi hari
/// live dari SATU batch kalender untuk semua karyawan + rentang tanggalnya.
async fn to_entities(
    conn: &mut PgConnection,
    company_id: i32,
    rows: &[OvertimeRequestRow],
    as_of: NaiveDateTime,
) -> ApiResult<Vec<OvertimeRequestEntity>> {
    let live: Vec<&OvertimeRequestRow> = rows.iter().filter(|r| r.day_type.is_none()).collect();
    let (Some(from), Some(to)) = (
        live.iter().map(|r| r.date).min(),
        live.iter().map(|r| r.date).max(),
    ) else {
        return Ok(rows
            .iter()
            .map(|r| to_overtime_request_entity(r, None))
            .collect());
    };
    let mut users: Vec<i32> = Vec::new();
    for r in &live {
        if !users.contains(&r.user_id) {
            users.push(r.user_id);
        }
    }
    let batch = load_batch(conn, company_id, from, to, &users).await?;
    let as_of_ms = ms_of(as_of);
    rows.iter()
        .map(|r| {
            if r.day_type.is_some() {
                return Ok(to_overtime_request_entity(r, None));
            }
            let info = day_info_for(&batch, r.user_id, r.date, as_of_ms)?;
            Ok(to_overtime_request_entity(r, Some(&live_day_of(&info))))
        })
        .collect()
}

/// Baris jalur jawab ORDER (`ORDER_SELECT`): + email pengaju (`users.username`).
#[derive(Debug, Clone, sqlx::FromRow)]
struct OrderRow {
    #[sqlx(flatten)]
    row: OvertimeRequestRow,
    requested_by_email: String,
}

/// `guardOrderResponse`: T5 settings FOR SHARE → baris ORDER milik pemanggil (selain itu
/// 404) → `PENDING_EMPLOYEE` → T10 tenggat. Semua lewat `tx`.
async fn guard_order_response(
    conn: &mut PgConnection,
    company_id: i32,
    user_id: i32,
    uuid: &str,
    now: NaiveDateTime,
) -> ApiResult<OrderRow> {
    locked_settings(&mut *conn, company_id).await?;
    let order: Option<OrderRow> = sqlx::query_as(&format!(
        r#"SELECT {OVERTIME_REQUEST_COLUMNS}, rb.username AS requested_by_email
           FROM {OVERTIME_REQUEST_FROM}
           WHERE r.uuid = $1::uuid AND r."companyId" = $2 AND r.kind = 'ORDER' AND r."userId" = $3
           LIMIT 1"#
    ))
    .bind(uuid)
    .bind(company_id)
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(order) = order else {
        return Err(not_found_error());
    };
    order_response_guard(order.row.status, order.row.response_deadline, now)?;
    Ok(order)
}

/// Sesudah commit, fire-and-forget: email ke approver pemberi perintah.
fn notify_order_decided(
    state: &AppState,
    order: &OrderRow,
    decision: OvertimeDecision,
    reject_reason: Option<&str>,
) {
    let row = &order.row;
    mail::decided(
        state,
        &OvertimeMailRecipient {
            email: order.requested_by_email.clone(),
            name: row.requested_by_name.clone(),
        },
        decision,
        &row.user_name,
        &row.user_name,
        &date_key_of(row.date),
        i64::from(row.requested_sec),
        reject_reason,
        Some(OvertimeRequestKind::ORDER),
    );
}

// ── route ────────────────────────────────────────────────────────────────────

/// `GET /quota`: pratinjau kuota form untuk pemanggil (US1 skenario 1).
pub async fn quota(
    state: &AppState,
    user_id: i32,
    query: QuotaQuery,
) -> ApiResult<DataResponse<QuotaEvaluation>> {
    let requested_sec = query.duration_minutes * 60;
    let company_id = require_company_id(&state.db, user_id).await?;
    assert_enabled(state, company_id).await?;

    let now = now_naive();
    let mut conn = state.db.acquire().await?;
    let clock = company_clock(&mut conn, company_id, now).await?;
    let evaluation = evaluate(
        &mut conn,
        &QuotaEvaluateInput {
            company_id,
            user_id,
            date: query.date,
            requested_sec,
            as_of_ms: ms_of(now),
            workweek_start_day: clock.workweek_start_day,
            exclude_request_id: None,
        },
    )
    .await?;
    Ok(DataResponse {
        data: evaluation.view,
    })
}

/// Hasil transaksi `submit` yang dipakai sesudah commit.
struct Submitted {
    row: OvertimeRequestRow,
    day: DayInfo,
    approvers: Vec<EffectiveApprover>,
    employee_name: String,
}

/// `POST /`: pengajuan SELF → `PENDING_APPROVAL`. Semua cek di DALAM satu transaksi; email
/// ke approver sesudah commit.
pub async fn submit(
    state: &AppState,
    user_id: i32,
    dto: CreateOvertimeRequestInput,
) -> ApiResult<DataResponse<OvertimeRequestEntity>> {
    let requested_sec = dto.duration_minutes * 60;
    let reason = dto.reason;
    if reason.is_empty() || reason.encode_utf16().count() > REASON_MAX_LENGTH {
        // Lapis kedua; DTO sudah men-trim dan menolak dengan 400.
        return Err(ApiError::bad_request(format!(
            "reason wajib diisi (maks {REASON_MAX_LENGTH} karakter)"
        )));
    }
    let date = dto.date;
    let date_key_str = date_key(date);

    let company_id = require_company_id(&state.db, user_id).await?;
    // Baris pengaturan harus ada supaya FOR SHARE di bawah benar-benar mengunci (T5).
    ensure_settings(&state.db, company_id).await?;
    let now = now_naive();
    let clock = {
        let mut conn = state.db.acquire().await?;
        company_clock(&mut conn, company_id, now).await?
    };

    let mut tx = state.db.begin().await?;
    // T5: FOR SHARE — tak bisa commit setelah disable yang tidak terlihat di sini.
    let retro_window_days = locked_settings(&mut tx, company_id).await?;

    // Dibaca di dalam tx; PUT settings (FOR UPDATE) tak bisa mengganti himpunan ini
    // selama FOR SHARE di atas dipegang.
    let approvers = effective_approvers(&mut *tx, company_id).await?;
    if approvers.is_empty() {
        return Err(overtime_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "OVERTIME_NO_APPROVER",
            "Belum ada approver lembur. Minta admin memilih approver di Pengaturan Lembur.",
            None,
        ));
    }

    if let Some(err) = retro_window_error(date, clock.local_today, retro_window_days) {
        return Err(err);
    }

    let employee = load_overtime_employee(&mut *tx, company_id, user_id).await?;
    if !is_eligible_overtime_employee(employee.as_ref()) {
        return Err(overtime_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "OVERTIME_EMPLOYEE_NOT_ELIGIBLE",
            "Akun Anda tidak dapat mengajukan lembur di perusahaan ini.",
            None,
        ));
    }
    let employee_name = employee.map(|e| e.name).unwrap_or_default();

    // T4: lock karyawan DULU, baru baca cuti (persetujuan cuti mengambil lock yang sama).
    lock_employee(&mut tx, company_id, user_id).await?;
    let batch = load_batch(&mut tx, company_id, date, date, &[user_id]).await?;
    let day = day_info_for(&batch, user_id, date, ms_of(now))?;
    day_guard(&day, requested_sec)?;

    // T3: kuota dievaluasi ulang di dalam tx; melebihi tanpa flag → 409 + evaluasi.
    let evaluation = evaluate(
        &mut tx,
        &QuotaEvaluateInput {
            company_id,
            user_id,
            date,
            requested_sec,
            as_of_ms: ms_of(now),
            workweek_start_day: clock.workweek_start_day,
            exclude_request_id: None,
        },
    )
    .await?;
    assert_confirmed(&evaluation, dto.confirm_over_quota)?;

    // T1: tanpa pra-cek baris lain — partial unique `overtimeRequest_one_active_per_day`
    // yang memutuskan (unique violation → 409 OVERTIME_DATE_TAKEN).
    let deadline = ndt(end_of_local_day_instant(
        &date_key_str,
        i64::from(retro_window_days),
        Some(&clock.time_zone),
    )
    .map_err(|e| internal(e.0))?);
    let requested_sec_i32 = i32::try_from(requested_sec).map_err(internal)?;
    let id: i32 = sqlx::query_scalar(
        r#"INSERT INTO "overtimeRequest" ("companyId", "userId", "requestedById", kind, date,
             "requestedSec", reason, status, "retroWindowDays", "responseDeadline", "finalAt",
             "createdAt", "updatedAt")
           VALUES ($1, $2, $2, 'SELF', $3, $4, $5, 'PENDING_APPROVAL', $6, $7, $7, $8, $8)
           RETURNING id"#,
    )
    .bind(company_id)
    .bind(user_id)
    .bind(date)
    .bind(requested_sec_i32)
    .bind(&reason)
    .bind(retro_window_days)
    .bind(deadline)
    .bind(now)
    .fetch_one(&mut *tx)
    .await
    .map_err(map_overtime_unique)?;
    let row = fetch_by_id(&mut tx, id).await?;
    tx.commit().await.map_err(map_overtime_unique)?;

    let created = Submitted {
        row,
        day,
        approvers,
        employee_name,
    };
    // Sesudah commit, fire-and-forget; pemanggil tak menerima email pengajuannya sendiri.
    let recipients: Vec<OvertimeMailRecipient> = created
        .approvers
        .iter()
        .filter(|a| a.id != user_id)
        .map(|a| OvertimeMailRecipient {
            email: a.email.clone(),
            name: a.name.clone(),
        })
        .collect();
    if !recipients.is_empty() {
        mail::request_submitted(
            state,
            &recipients,
            &created.employee_name,
            &date_key_str,
            requested_sec,
            &reason,
        );
    }

    Ok(DataResponse {
        data: to_overtime_request_entity(&created.row, Some(&live_day_of(&created.day))),
    })
}

/// `GET /me`: baris milik pemanggil sebagai karyawan (`userId`), semua status, termasuk
/// ORDER yang ditujukan kepadanya (FR-018).
pub async fn list_mine(
    state: &AppState,
    user_id: i32,
    query: ListMineQuery,
) -> ApiResult<ListResponse> {
    let ListMineQuery {
        statuses,
        from,
        to,
        page,
        limit,
    } = query;
    let company_id = require_company_id(&state.db, user_id).await?;
    assert_enabled(state, company_id).await?;

    let push_where = |qb: &mut QueryBuilder<'_, Postgres>| {
        qb.push(r#" WHERE r."companyId" = "#)
            .push_bind(company_id)
            .push(r#" AND r."userId" = "#)
            .push_bind(user_id);
        if !statuses.is_empty() {
            let list: Vec<&str> = statuses.iter().map(OvertimeRequestStatus::as_str).collect();
            qb.push(r#" AND r.status = ANY("#)
                .push_bind(list)
                .push(r#"::"OvertimeRequestStatus"[])"#);
        }
        if let Some(from) = from {
            qb.push(" AND r.date >= ").push_bind(from);
        }
        if let Some(to) = to {
            qb.push(" AND r.date <= ").push_bind(to);
        }
    };

    let mut count_qb = QueryBuilder::<Postgres>::new(r#"SELECT COUNT(*) FROM "overtimeRequest" r"#);
    push_where(&mut count_qb);
    let total: i64 = count_qb.build_query_scalar().fetch_one(&state.db).await?;

    let mut rows_qb = QueryBuilder::<Postgres>::new(format!(
        "SELECT {OVERTIME_REQUEST_COLUMNS} FROM {OVERTIME_REQUEST_FROM}"
    ));
    push_where(&mut rows_qb);
    rows_qb
        .push(" ORDER BY r.date DESC, r.id DESC OFFSET ")
        .push_bind((page - 1) * limit)
        .push(" LIMIT ")
        .push_bind(limit);
    let rows: Vec<OvertimeRequestRow> = rows_qb.build_query_as().fetch_all(&state.db).await?;

    let mut conn = state.db.acquire().await?;
    let data = to_entities(&mut conn, company_id, &rows, now_naive()).await?;
    Ok(ListResponse {
        data,
        meta: PaginationMeta {
            total,
            page,
            limit,
            total_pages: total_pages(total, limit),
        },
    })
}

/// `GET /:id`: hanya karyawan atau pengaju; selain itu (termasuk company lain) 404.
pub async fn get_one(
    state: &AppState,
    user_id: i32,
    uuid: &str,
) -> ApiResult<DataResponse<OvertimeRequestEntity>> {
    let company_id = require_company_id(&state.db, user_id).await?;
    assert_enabled(state, company_id).await?;
    if !is_uuid_any(uuid) {
        return Err(not_found_error());
    }
    let row: Option<OvertimeRequestRow> = sqlx::query_as(&format!(
        r#"SELECT {OVERTIME_REQUEST_COLUMNS} FROM {OVERTIME_REQUEST_FROM}
           WHERE r.uuid = $1::uuid AND r."companyId" = $2
             AND (r."userId" = $3 OR r."requestedById" = $3)
           LIMIT 1"#
    ))
    .bind(uuid)
    .bind(company_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else {
        return Err(not_found_error());
    };
    let mut conn = state.db.acquire().await?;
    let entity = to_entities(
        &mut conn,
        company_id,
        std::slice::from_ref(&row),
        now_naive(),
    )
    .await?
    .into_iter()
    .next()
    .ok_or_else(not_found_error)?;
    Ok(DataResponse { data: entity })
}

/// `POST /:id/cancel`: hanya pengaju (`requestedById`), hanya `PENDING_*` sebelum
/// `responseDeadline` → `CANCELLED BY_REQUESTER` (FR-021).
pub async fn cancel(
    state: &AppState,
    user_id: i32,
    uuid: &str,
) -> ApiResult<DataResponse<OvertimeRequestEntity>> {
    let company_id = require_company_id(&state.db, user_id).await?;
    if !is_uuid_any(uuid) {
        return Err(not_found_error());
    }
    let now = now_naive();

    let mut tx = state.db.begin().await?;
    // T5: FOR SHARE, sama dengan jalur tulis lain (tanpa ensureSettings, seperti Node).
    locked_settings(&mut tx, company_id).await?;

    // T2 + T121: CAS pada status + pengaju + tenggat.
    let updated = sqlx::query(
        r#"UPDATE "overtimeRequest"
              SET status = 'CANCELLED', "cancelReason" = 'BY_REQUESTER', "cancelledAt" = $4,
                  "cancelledById" = $3, "updatedAt" = $4
            WHERE uuid = $1::uuid AND "companyId" = $2 AND "requestedById" = $3
              AND status IN ('PENDING_APPROVAL', 'PENDING_EMPLOYEE')
              AND "responseDeadline" > $4"#,
    )
    .bind(uuid)
    .bind(company_id)
    .bind(user_id)
    .bind(now)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // Baca ulang: bentuk respons saat sukses, pembeda 404/403/409 saat gagal.
    let current: Option<OvertimeRequestRow> = sqlx::query_as(&format!(
        r#"SELECT {OVERTIME_REQUEST_COLUMNS} FROM {OVERTIME_REQUEST_FROM}
           WHERE r.uuid = $1::uuid AND r."companyId" = $2 LIMIT 1"#
    ))
    .bind(uuid)
    .bind(company_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(current) = current else {
        return Err(not_found_error());
    };
    if current.user_id != user_id && current.requested_by_id != user_id {
        // US4 skenario 6: approver lain yang mencoba membatalkan ORDER → 403 (bukan 404).
        // Himpunan approver hanya dibaca di jalur gagal ini, di dalam tx (bawah FOR SHARE).
        if current.kind == OvertimeRequestKind::ORDER {
            let approvers = effective_approvers(&mut *tx, company_id).await?;
            if approvers.iter().any(|a| a.id == user_id) {
                return Err(not_requester_error());
            }
        }
        return Err(not_found_error());
    }
    if let Some(err) = cancel_failure(&current, user_id, updated == 1, now) {
        return Err(err);
    }
    tx.commit().await?;

    let mut conn = state.db.acquire().await?;
    let entity = to_entities(&mut conn, company_id, std::slice::from_ref(&current), now)
        .await?
        .into_iter()
        .next()
        .ok_or_else(not_found_error)?;
    Ok(DataResponse { data: entity })
}

/// `POST /:id/accept`: karyawan yang dituju menerima ORDER (`PENDING_EMPLOYEE` →
/// `APPROVED`, `decidedById` = karyawan). Urutan penjaga: T5 → baris → status → T10 → T3 →
/// T2. Email ke approver pemberi perintah setelah commit.
pub async fn accept(
    state: &AppState,
    user_id: i32,
    uuid: &str,
    confirm_over_quota: Option<bool>,
) -> ApiResult<DataResponse<OvertimeRequestEntity>> {
    if !is_uuid_any(uuid) {
        return Err(not_found_error());
    }
    let company_id = require_company_id(&state.db, user_id).await?;
    ensure_settings(&state.db, company_id).await?;
    let now = now_naive();
    let clock = {
        let mut conn = state.db.acquire().await?;
        company_clock(&mut conn, company_id, now).await?
    };

    let mut tx = state.db.begin().await?;
    let order = guard_order_response(&mut tx, company_id, user_id, uuid, now).await?;
    let date = order.row.date;

    // Tenggat ORDER = akhir tanggalnya: tak ada hitung di tempat.
    let batch = load_batch(&mut tx, company_id, date, date, &[user_id]).await?;
    let info = day_info_for(&batch, user_id, date, ms_of(now))?;

    // T3: kuota dievaluasi ulang DI DALAM tx.
    let evaluation = evaluate(
        &mut tx,
        &QuotaEvaluateInput {
            company_id,
            user_id,
            date,
            requested_sec: i64::from(order.row.requested_sec),
            as_of_ms: ms_of(now),
            workweek_start_day: clock.workweek_start_day,
            exclude_request_id: Some(order.row.id),
        },
    )
    .await?;
    assert_confirmed(&evaluation, confirm_over_quota)?;

    // T2: CAS pada status.
    let exceeds_daily = evaluation.view.exceeds_daily;
    let updated = sqlx::query(
        r#"UPDATE "overtimeRequest"
              SET status = 'APPROVED', "approvedAt" = $2, "decidedById" = $3, "decidedAt" = $2,
                  "dayType" = $4::"OvertimeDayType", "baseNormalSec" = $5, "dailyLimitSec" = $6,
                  "patternWorkDays" = $7, "exceedsDailyLimit" = $8, "exceedsLimit" = $8,
                  "updatedAt" = $2
            WHERE id = $1 AND status = 'PENDING_EMPLOYEE'"#,
    )
    .bind(order.row.id)
    .bind(now)
    .bind(user_id)
    .bind(day_type_str(info.day_type))
    .bind(i32::try_from(info.base_normal_sec).map_err(internal)?)
    .bind(i32::try_from(info.daily_limit_sec).map_err(internal)?)
    .bind(i16::try_from(info.pattern_work_days).map_err(internal)?)
    // Label mingguan baru dihitung setelah komputasi (R7).
    .bind(exceeds_daily)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(state_conflict_error());
    }
    let fresh = fetch_by_id(&mut tx, order.row.id).await?;
    tx.commit().await?;

    notify_order_decided(state, &order, OvertimeDecision::Approved, None);
    Ok(DataResponse {
        data: to_overtime_request_entity(&fresh, None),
    })
}

/// `POST /:id/reject`: karyawan menolak ORDER (`PENDING_EMPLOYEE` → `REJECTED`); alasan
/// wajib. Email ke approver pemberi perintah setelah commit.
pub async fn reject(
    state: &AppState,
    user_id: i32,
    uuid: &str,
    reason: String,
) -> ApiResult<DataResponse<OvertimeRequestEntity>> {
    if reason.is_empty() {
        // Lapis kedua; DTO sudah menolak dengan 400.
        return Err(ApiError::bad_request("Alasan penolakan wajib diisi."));
    }
    if !is_uuid_any(uuid) {
        return Err(not_found_error());
    }
    let company_id = require_company_id(&state.db, user_id).await?;
    ensure_settings(&state.db, company_id).await?;
    let now = now_naive();

    let mut tx = state.db.begin().await?;
    let order = guard_order_response(&mut tx, company_id, user_id, uuid, now).await?;
    // T2: CAS pada status.
    let updated = sqlx::query(
        r#"UPDATE "overtimeRequest"
              SET status = 'REJECTED', "rejectReason" = $2, "decidedById" = $3, "decidedAt" = $4,
                  "updatedAt" = $4
            WHERE id = $1 AND status = 'PENDING_EMPLOYEE'"#,
    )
    .bind(order.row.id)
    .bind(&reason)
    .bind(user_id)
    .bind(now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(state_conflict_error());
    }
    let fresh = fetch_by_id(&mut tx, order.row.id).await?;
    tx.commit().await?;

    notify_order_decided(state, &order, OvertimeDecision::Rejected, Some(&reason));
    Ok(DataResponse {
        data: to_overtime_request_entity(&fresh, None),
    })
}

#[cfg(test)]
mod tests {
    //! Kasus murni `overtime-requests.service.spec.ts` (yang tak butuh DB).
    #![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use axum::response::IntoResponse;

    fn ts(s: &str) -> NaiveDateTime {
        mz_core::time::parse_js_date(s).unwrap()
    }

    fn d(s: &str) -> NaiveDate {
        crate::zone::parse_date_key(s).unwrap()
    }

    async fn body_of(e: ApiError) -> (u16, serde_json::Value) {
        let res = e.into_response();
        let status = res.status().as_u16();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn row(
        status: OvertimeRequestStatus,
        user: i32,
        requester: i32,
        deadline: &str,
    ) -> OvertimeRequestRow {
        OvertimeRequestRow {
            id: 1,
            uuid: "11111111-1111-4111-8111-111111111111".into(),
            user_id: user,
            requested_by_id: requester,
            decided_by_id: None,
            kind: OvertimeRequestKind::SELF,
            status,
            date: d("2026-10-22"),
            requested_sec: 7200,
            reason: "x".into(),
            response_deadline: ts(deadline),
            final_at: ts(deadline),
            decided_at: None,
            reject_reason: None,
            cancel_reason: None,
            day_type: None,
            base_normal_sec: None,
            daily_limit_sec: None,
            normal_sec: None,
            worked_sec: None,
            overtime_sec: None,
            computed_at: None,
            finalized_at: None,
            exceeds_limit: false,
            exceeds_daily_limit: false,
            exceeds_weekly_limit: false,
            created_at: ts("2026-10-21T08:00:00.000Z"),
            user_uuid: "u".into(),
            user_name: "Budi".into(),
            requested_by_uuid: "u".into(),
            requested_by_name: "Budi".into(),
            decided_by_uuid: None,
            decided_by_name: None,
        }
    }

    fn day(day_type: DayType, base: i64, leave: LeaveCoverage) -> DayInfo {
        DayInfo {
            day_type,
            base_normal_sec: base,
            daily_limit_sec: 14_400,
            pattern_work_days: 5,
            leave,
        }
    }

    #[tokio::test]
    async fn retro_window() {
        let today = d("2026-10-21");
        assert!(retro_window_error(d("2026-10-14"), today, 7).is_none());
        assert!(retro_window_error(d("2026-11-30"), today, 7).is_none());
        let (status, body) = body_of(retro_window_error(d("2026-10-13"), today, 7).unwrap()).await;
        assert_eq!(status, 422);
        assert_eq!(
            body,
            json!({
                "code": "OVERTIME_RETRO_WINDOW_PASSED",
                "message": "Tanggal ini sudah melewati batas pengajuan mundur (7 hari).",
                "details": {"earliestDate": "2026-10-14", "retroWindowDays": 7}
            })
        );
        // Jendela 0: hanya hari ini dan sesudahnya.
        assert!(retro_window_error(d("2026-10-20"), today, 0).is_some());
        assert!(retro_window_error(today, today, 0).is_none());
    }

    #[tokio::test]
    async fn fx_7_3_day_guard() {
        // Hari libur (normal 0): 16 jam lolos; hari kerja 8 jam: 16 jam lolos.
        assert!(day_guard(&day(DayType::Holiday, 0, LeaveCoverage::None), 16 * 3600).is_ok());
        assert!(
            day_guard(
                &day(DayType::Workday, 28_800, LeaveCoverage::None),
                16 * 3600
            )
            .is_ok()
        );
        // 16 jam 1 menit → 400.
        let err = day_guard(
            &day(DayType::Workday, 28_800, LeaveCoverage::None),
            16 * 3600 + 60,
        )
        .unwrap_err();
        let (status, body) = body_of(err).await;
        assert_eq!(status, 400);
        assert_eq!(body["code"], "OVERTIME_DURATION_EXCEEDS_DAY");
        let err = day_guard(
            &day(DayType::Workday, 28_800, LeaveCoverage::None),
            17 * 3600,
        )
        .unwrap_err();
        let (_, body) = body_of(err).await;
        assert_eq!(
            body["message"],
            "Lembur 17 jam + jam normal 8 jam melebihi 24 jam."
        );
        // Cuti setengah hari tidak menolak; cuti penuh → 422.
        assert!(day_guard(&day(DayType::Workday, 28_800, LeaveCoverage::Half), 3600).is_ok());
        let err = day_guard(&day(DayType::Workday, 28_800, LeaveCoverage::Full), 3600).unwrap_err();
        let (status, body) = body_of(err).await;
        assert_eq!(status, 422);
        assert_eq!(body["code"], "OVERTIME_EMPLOYEE_ON_LEAVE");
    }

    #[tokio::test]
    async fn cancel_failures() {
        let now = ts("2026-10-25T00:00:00.000Z");
        let future = "2026-10-29T16:59:59.999Z";
        let past = "2026-10-24T16:59:59.999Z";
        // CAS berhasil.
        assert!(
            cancel_failure(
                &row(OvertimeRequestStatus::PENDING_APPROVAL, 7, 7, future),
                7,
                true,
                now
            )
            .is_none()
        );
        // Karyawan ORDER (bukan pengaju) → 403.
        let (s, b) = body_of(
            cancel_failure(
                &row(OvertimeRequestStatus::PENDING_EMPLOYEE, 7, 9, future),
                7,
                false,
                now,
            )
            .unwrap(),
        )
        .await;
        assert_eq!(
            (s, b["code"].as_str()),
            (403, Some("OVERTIME_NOT_REQUESTER"))
        );
        // APPROVED → 409 STATE_CONFLICT + status.
        let (s, b) = body_of(
            cancel_failure(
                &row(OvertimeRequestStatus::APPROVED, 7, 7, future),
                7,
                false,
                now,
            )
            .unwrap(),
        )
        .await;
        assert_eq!(s, 409);
        assert_eq!(
            b,
            json!({"code": "OVERTIME_STATE_CONFLICT", "message": "Pengajuan lembur ini sudah tidak dapat dibatalkan.", "details": {"status": "APPROVED"}})
        );
        // T121: lewat tenggat, masih PENDING → 409 DEADLINE_PASSED.
        let (s, b) = body_of(
            cancel_failure(
                &row(OvertimeRequestStatus::PENDING_APPROVAL, 7, 7, past),
                7,
                false,
                now,
            )
            .unwrap(),
        )
        .await;
        assert_eq!(
            (s, b["code"].as_str()),
            (409, Some("OVERTIME_DEADLINE_PASSED"))
        );
        // CAS 0 baris walau masih pending sebelum tenggat (balapan) → STATE_CONFLICT.
        let (_, b) = body_of(
            cancel_failure(
                &row(OvertimeRequestStatus::PENDING_APPROVAL, 7, 7, future),
                7,
                false,
                now,
            )
            .unwrap(),
        )
        .await;
        assert_eq!(b["code"], "OVERTIME_STATE_CONFLICT");
    }

    #[tokio::test]
    async fn order_guard() {
        let now = ts("2026-10-25T00:00:00.000Z");
        assert!(
            order_response_guard(
                OvertimeRequestStatus::PENDING_EMPLOYEE,
                ts("2026-10-25T00:00:00.001Z"),
                now
            )
            .is_ok()
        );
        let (s, b) = body_of(
            order_response_guard(
                OvertimeRequestStatus::APPROVED,
                ts("2026-10-26T00:00:00.000Z"),
                now,
            )
            .unwrap_err(),
        )
        .await;
        assert_eq!(s, 409);
        assert_eq!(
            b,
            json!({"code": "OVERTIME_STATE_CONFLICT", "message": "Status pengajuan lembur sudah berubah. Muat ulang lalu coba lagi."})
        );
        // T10: now >= responseDeadline.
        let (s, b) = body_of(
            order_response_guard(OvertimeRequestStatus::PENDING_EMPLOYEE, now, now).unwrap_err(),
        )
        .await;
        assert_eq!(s, 409);
        assert_eq!(
            b["message"],
            "Batas waktu menjawab perintah lembur ini sudah lewat."
        );
    }

    #[test]
    fn pages() {
        assert_eq!(total_pages(0, 20), 0);
        assert_eq!(total_pages(1, 20), 1);
        assert_eq!(total_pages(40, 20), 2);
        assert_eq!(total_pages(41, 20), 3);
    }
}
