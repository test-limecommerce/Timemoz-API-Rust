//! `MobileLeavesService` (`mobile-leaves.service.ts`): saldo sendiri, pengajuan (dengan
//! lampiran GCS), daftar/detail/dokumen milik sendiri, revisi, pembatalan, pratinjau hari.

use chrono::{Datelike, NaiveDate};
use mz_core::{ApiError, ApiResult, AppState};
use mz_leave::balance::{find_balance, get_or_create_balance};
use mz_leave::calendar::{HolidayIndex, load_holiday_index, off_day_predicate};
use mz_leave::day_count::{
    DayCount, ExclusionReason, count_leave_days, date_key_of, parse_date_key,
};
use mz_leave::enums::{LeaveDayType, LeaveSickMode, LeaveStatus};
use mz_leave::math::{BalanceAmounts, available, requestable, resolve_sick_mode};
use mz_leave::policy::{
    get_policy_approvers, load_entitlement_of, load_policy, local_year, policy_carry_over_expired,
    require_time_zone,
};
use mz_leave::view::{BalanceViewCtx, get_scheduled_holiday_deductions, to_balance_view};
use serde_json::{Value, json};

use crate::dto::{DayCountQuery, MyRequestsQuery, Opt, ReviseDto, SubmitDto};
use crate::form::Document;
use crate::model::{
    Include, LEAVE_TYPE_COLUMNS, LeaveTypeConfig, RequestRow, RequestView, fetch_by_id,
    select_where,
};
use crate::notify;
use crate::util::{invalid_date, iso_year_bounds, require_db_uuid};

fn num(x: f64) -> Value {
    mz_core::jsfmt::num(x)
}

fn no_working_days() -> ApiError {
    ApiError::coded(
        http::StatusCode::BAD_REQUEST,
        "LEAVE_NO_WORKING_DAYS",
        "Rentang tanggal tidak berisi hari kerja",
    )
}

/// Kunci tanggal yang sudah lolos [`count_days`] → `NaiveDate`.
fn key_date(key: &str) -> ApiResult<NaiveDate> {
    parse_date_key(key).ok_or_else(invalid_date)
}

/// `countDays(companyId, startKey, endKey, startDayType, endDayType)`: libur mingguan pola kerja
/// (bawaan Sabtu–Minggu) dan libur aktif tidak dihitung; 0 hari → 400 `LEAVE_NO_WORKING_DAYS`.
async fn count_days(
    state: &AppState,
    company_id: i32,
    start_key: &str,
    end_key: &str,
    start_day_type: LeaveDayType,
    end_day_type: LeaveDayType,
) -> ApiResult<(DayCount, HolidayIndex)> {
    // Kunci bukan `YYYY-MM-DD` (bentuk ISO lain yang lolos validator) = `Invalid Date` ke
    // Prisma di `loadHolidayIndex` → 500, sebelum `countLeaveDays`.
    let (Some(from), Some(to)) = (parse_date_key(start_key), parse_date_key(end_key)) else {
        return Err(invalid_date());
    };
    let mut conn = state.db.acquire().await?;
    let index = load_holiday_index(&mut conn, company_id, from, to).await?;
    let off =
        off_day_predicate(&mut conn, company_id, from, to, mz_core::time::now_naive()).await?;
    drop(conn);
    let is_off = |d| off.is_off(d);
    let result = count_leave_days(
        start_key,
        end_key,
        start_day_type,
        end_day_type,
        &index.excluded_dates,
        Some(&is_off),
    )?;
    if result.total_days == 0.0 {
        return Err(no_working_days());
    }
    Ok((result, index))
}

/// `requestableFor(userId, companyId, year)`: tanpa baris saldo, baris hanya dibuat untuk tahun
/// berjalan ±1; di luar itu pratinjau GET tidak menulis baris baru (0).
async fn requestable_for(
    state: &AppState,
    user_id: i32,
    company_id: i32,
    year: i32,
) -> ApiResult<f64> {
    let tz = require_time_zone(&state.db, company_id).await?;
    let now = mz_core::time::now_naive();
    let balance = match find_balance(&state.db, user_id, company_id, year).await? {
        Some(b) => b,
        None => {
            if (i64::from(year) - i64::from(local_year(now, &tz))).abs() > 1 {
                return Ok(0.0);
            }
            get_or_create_balance(&state.db, user_id, company_id, year).await?
        }
    };
    let policy = load_policy(&state.db, company_id).await?;
    let mut conn = state.db.acquire().await?;
    let scheduled =
        get_scheduled_holiday_deductions(&mut conn, user_id, company_id, year, &tz, now).await?;
    let expired = policy_carry_over_expired(policy.as_ref(), year, &tz, now);
    #[allow(clippy::cast_precision_loss)]
    Ok(requestable(
        available(&BalanceAmounts::from(&balance), expired),
        scheduled.len() as f64,
    ))
}

/// `getDayCountPreview(userId, query)`.
pub async fn day_count_preview(
    state: &AppState,
    user_id: i32,
    company_id: i32,
    q: &DayCountQuery,
) -> ApiResult<Value> {
    let start_key = date_key_of(&q.start_date);
    let end_key = date_key_of(&q.end_date);
    let (count, index) = count_days(
        state,
        company_id,
        start_key,
        end_key,
        q.start_day_type.unwrap_or(LeaveDayType::FULL_DAY),
        q.end_day_type.unwrap_or(LeaveDayType::FULL_DAY),
    )
    .await?;
    let excluded: Vec<Value> = count
        .excluded
        .iter()
        .map(|e| {
            let holiday = (e.reason == ExclusionReason::Holiday)
                .then(|| parse_date_key(&e.date).and_then(|d| index.by_date.get(&d)))
                .flatten();
            match holiday {
                Some(h) => json!({
                    "date": e.date, "reason": e.reason, "name": h.name, "deductsLeave": h.deducts_leave,
                }),
                None => json!({ "date": e.date, "reason": e.reason }),
            }
        })
        .collect();
    let year = start_key
        .get(..4)
        .and_then(|y| y.parse::<i32>().ok())
        .ok_or_else(invalid_date)?;
    Ok(json!({
        "totalDays": num(count.total_days),
        "excludedDates": excluded,
        "requestable": num(requestable_for(state, user_id, company_id, year).await?),
    }))
}

/// `companyLeaveType.findFirst({ id, companyId, isActive: true })`.
async fn active_leave_type(
    state: &AppState,
    company_id: i32,
    id: i64,
) -> ApiResult<Option<LeaveTypeConfig>> {
    Ok(sqlx::query_as(&format!(
        r#"SELECT {LEAVE_TYPE_COLUMNS} FROM "companyLeaveType" lt
           WHERE lt.id = $1 AND lt."companyId" = $2 AND lt."isActive" = true LIMIT 1"#
    ))
    .bind(id)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?)
}

/// Jumlah `totalDays` pengajuan PENDING/APPROVED jenis `type_id` yang mulai di tahun `year`.
async fn used_quota(
    state: &AppState,
    user_id: i32,
    company_id: i32,
    type_id: i32,
    year: i64,
) -> ApiResult<f64> {
    let (from, to) = iso_year_bounds(year).ok_or_else(invalid_date)?;
    let used: Option<f64> = sqlx::query_scalar(
        r#"SELECT SUM("totalDays") FROM "leaveRequest"
           WHERE "userId" = $1 AND "companyId" = $2 AND "leaveTypeId" = $3
             AND status IN ('PENDING'::"LeaveStatus", 'APPROVED'::"LeaveStatus")
             AND "startDate" >= $4 AND "startDate" <= $5"#,
    )
    .bind(user_id)
    .bind(company_id)
    .bind(type_id)
    .bind(from)
    .bind(to)
    .fetch_one(&state.db)
    .await?;
    Ok(used.unwrap_or(0.0))
}

/// `validateBalance(userId, companyId, leaveTypeConfig, totalDays, year)`: kuota tahunan per
/// jenis lalu saldo (tanpa kunci: pemeriksaan awal; persetujuan memeriksa ulang di bawah kunci).
async fn validate_balance(
    state: &AppState,
    user_id: i32,
    company_id: i32,
    cfg: &LeaveTypeConfig,
    total_days: f64,
    year: i32,
) -> ApiResult<()> {
    if let Some(quota) = cfg.annual_quota {
        let used = used_quota(state, user_id, company_id, cfg.id, i64::from(year)).await?;
        let remaining = f64::from(quota) - used;
        if total_days > remaining {
            return Err(ApiError::bad_request(format!(
                "Kuota {} tidak cukup. Tersisa: {} hari.",
                cfg.name,
                mz_core::jsfmt::num_string(remaining.max(0.0))
            )));
        }
    }
    if !cfg.deduct_balance {
        return Ok(());
    }
    let balance = get_or_create_balance(&state.db, user_id, company_id, year).await?;
    let policy = load_policy(&state.db, company_id).await?;
    let tz = require_time_zone(&state.db, company_id).await?;
    let now = mz_core::time::now_naive();

    let check_annual = async |label: &str| -> ApiResult<()> {
        let expired = policy_carry_over_expired(policy.as_ref(), year, &tz, now);
        let mut conn = state.db.acquire().await?;
        let reserved =
            get_scheduled_holiday_deductions(&mut conn, user_id, company_id, year, &tz, now)
                .await?
                .len();
        #[allow(clippy::cast_precision_loss)]
        let left = requestable(
            available(&BalanceAmounts::from(&balance), expired),
            reserved as f64,
        );
        if total_days > left {
            let suffix = if reserved > 0 {
                format!(" ({reserved} hari dicadangkan untuk cuti bersama)")
            } else {
                String::new()
            };
            return Err(ApiError::bad_request(format!(
                "{label} tidak cukup. Tersisa: {} hari{suffix}.",
                mz_core::jsfmt::num_string(left)
            )));
        }
        Ok(())
    };

    if cfg.balance_source == "SICK" {
        match resolve_sick_mode(policy.as_ref()) {
            LeaveSickMode::LIMITED => {
                let avail = (balance.sick_leave_total - balance.sick_leave_used).max(0.0);
                if total_days > avail {
                    return Err(ApiError::bad_request(format!(
                        "Saldo cuti sakit tidak cukup. Tersisa: {} hari.",
                        mz_core::jsfmt::num_string(avail)
                    )));
                }
            }
            LeaveSickMode::DEDUCT_ANNUAL => check_annual("Saldo cuti").await?,
            // UNLIMITED: tanpa batas jumlah cuti sakit.
            LeaveSickMode::UNLIMITED => {}
        }
    } else {
        check_annual("Saldo cuti tahunan").await?;
    }
    Ok(())
}

/// `getPolicyApprovers(companyId)` memuat `approver_id`?
async fn is_valid_approver(state: &AppState, company_id: i32, approver_id: i64) -> ApiResult<bool> {
    Ok(get_policy_approvers(&state.db, company_id)
        .await?
        .iter()
        .any(|a| i64::from(a.id) == approver_id))
}

/// Unggah lampiran: `leave-documents/<companyId>/<userId>/<Date.now()>.<ext>`.
async fn upload_document(
    state: &AppState,
    company_id: i32,
    user_id: i32,
    doc: &Document,
) -> ApiResult<String> {
    let key = format!(
        "leave-documents/{company_id}/{user_id}/{}.{}",
        chrono::Utc::now().timestamp_millis(),
        doc.ext()
    );
    mz_files::upload_buffer(state, &key, doc.bytes.clone(), &doc.mime).await?;
    Ok(key)
}

/// Pengingat 48 jam untuk pengajuan PENDING (`enqueueReminder(id, (updatedAt ?? createdAt) ms)`).
async fn enqueue_reminder(state: &AppState, r: &RequestRow) {
    if r.status == LeaveStatus::PENDING.as_str() {
        let at = r.updated_at.unwrap_or(r.created_at);
        mz_leave::jobs::enqueue_reminder(state, r.id, at.and_utc().timestamp_millis()).await;
    }
}

fn i32_of(v: i64) -> ApiResult<i32> {
    i32::try_from(v).map_err(|_| ApiError::internal(anyhow::anyhow!("Int out of range: {v}")))
}

/// `submitLeaveRequest(userId, dto, document)`.
pub async fn submit(
    state: &AppState,
    user_id: i32,
    company_id: i32,
    dto: &SubmitDto,
    document: Option<&Document>,
) -> ApiResult<RequestView> {
    let cfg = active_leave_type(state, company_id, dto.leave_type_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Jenis cuti tidak ditemukan atau tidak aktif"))?;
    if cfg.requires_document && document.is_none() {
        return Err(ApiError::bad_request(format!(
            "{} wajib melampirkan dokumen",
            cfg.name
        )));
    }
    if !is_valid_approver(state, company_id, dto.approver_id).await? {
        return Err(ApiError::bad_request(
            "Approver tidak valid untuk perusahaan ini",
        ));
    }

    let start_key = date_key_of(&dto.start_date).to_owned();
    let end_key = date_key_of(&dto.end_date).to_owned();
    let start_day_type = dto.start_day_type.unwrap_or(LeaveDayType::FULL_DAY);
    let end_day_type = dto.end_day_type.unwrap_or(LeaveDayType::FULL_DAY);
    let (count, _) = count_days(
        state,
        company_id,
        &start_key,
        &end_key,
        start_day_type,
        end_day_type,
    )
    .await?;
    let total_days = count.total_days;
    let start = key_date(&start_key)?;
    let end = key_date(&end_key)?;
    let year = start.year();

    validate_balance(state, user_id, company_id, &cfg, total_days, year).await?;

    let document_key = match document {
        Some(doc) => Some(upload_document(state, company_id, user_id, doc).await?),
        None => None,
    };

    let id: i32 = sqlx::query_scalar(
        r#"INSERT INTO "leaveRequest"
             ("userId", "companyId", "leaveTypeId", "approverId", "startDate", "endDate",
              "startDayType", "endDayType", "totalDays", reason, "documentKey", status)
           VALUES ($1, $2, $3, $4, $5, $6, $7::"LeaveDayType", $8::"LeaveDayType", $9, $10, $11,
                   'PENDING'::"LeaveStatus")
           RETURNING id"#,
    )
    .bind(user_id)
    .bind(company_id)
    .bind(cfg.id)
    .bind(i32_of(dto.approver_id)?)
    .bind(start)
    .bind(end)
    .bind(start_day_type.as_str())
    .bind(end_day_type.as_str())
    .bind(total_days)
    .bind(&dto.reason)
    .bind(document_key.as_deref())
    .fetch_one(&state.db)
    .await?;
    let created = fetch_by_id(&state.db, id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;

    enqueue_reminder(state, &created).await;

    // Sesudah commit; gagal kirim email tidak boleh menggagalkan pengajuan.
    if let (Some(email), Some(name)) = (
        created.ap_username.clone().filter(|e| !e.is_empty()),
        created.ap_name.clone(),
    ) {
        let leave_type_name = created.lt_name.clone().unwrap_or_else(|| cfg.name.clone());
        let mut details = notify::details(&created, leave_type_name);
        details.start_date = start_key;
        details.end_date = end_key;
        details.reason = dto.reason.clone();
        details.total_days = total_days;
        notify::request_to_approver(state, details, email, name);
    }
    Ok(created.view(Include::SUBMIT))
}

/// `getApprovers(userId)`.
pub async fn approvers(state: &AppState, company_id: i32) -> ApiResult<Value> {
    let list = get_policy_approvers(&state.db, company_id).await?;
    serde_json::to_value(list).map_err(ApiError::internal)
}

/// `getMyBalance(userId)`.
pub async fn my_balance(state: &AppState, user_id: i32, company_id: i32) -> ApiResult<Value> {
    let now = mz_core::time::now_naive();
    // `new Date().getFullYear()` (zona proses = UTC).
    let year = now.year();
    let balance = get_or_create_balance(&state.db, user_id, company_id, year).await?;
    let policy = load_policy(&state.db, company_id).await?;
    let mut conn = state.db.acquire().await?;
    let (override_days, tier) = load_entitlement_of(&mut conn, user_id, company_id).await?;
    let tz = require_time_zone(&mut *conn, company_id).await?;
    let scheduled =
        get_scheduled_holiday_deductions(&mut conn, user_id, company_id, year, &tz, now).await?;
    drop(conn);
    let sick_mode = resolve_sick_mode(policy.as_ref());
    let view = to_balance_view(
        &balance,
        &BalanceViewCtx {
            policy: policy.as_ref(),
            override_days,
            tier_days: tier.as_ref().map(|t| t.annual_leave_days),
            time_zone: &tz,
            scheduled_holiday_days: scheduled.len(),
            now,
        },
    );

    let types: Vec<(i32, String, i32)> = sqlx::query_as(
        r#"SELECT id, name, "annualQuota" FROM "companyLeaveType"
           WHERE "companyId" = $1 AND "isActive" = true AND "annualQuota" IS NOT NULL
           ORDER BY id"#,
    )
    .bind(company_id)
    .fetch_all(&state.db)
    .await?;
    let mut quotas = Vec::with_capacity(types.len());
    for (id, name, quota) in types {
        let used = used_quota(state, user_id, company_id, id, i64::from(year)).await?;
        quotas.push(json!({
            "leaveTypeId": id,
            "name": name,
            "quota": quota,
            "used": num(used),
            "remaining": num((f64::from(quota) - used).max(0.0)),
        }));
    }

    let b = &balance;
    let sick_total = if sick_mode == LeaveSickMode::UNLIMITED {
        0.0
    } else {
        b.sick_leave_total
    };
    let sick_remaining = match sick_mode {
        LeaveSickMode::UNLIMITED => 0.0,
        LeaveSickMode::LIMITED => (b.sick_leave_total - b.sick_leave_used).max(0.0),
        LeaveSickMode::DEDUCT_ANNUAL => b.sick_leave_total - b.sick_leave_used,
    };
    Ok(json!({
        "year": year,
        "annualLeave": {
            "total": num(b.annual_leave_total),
            "used": num(b.annual_leave_used),
            "remaining": num(b.annual_leave_total - b.annual_leave_used),
            "accrualMode": view.accrual_mode,
            "entitlement": num(view.entitlement),
            "requestable": num(view.requestable),
        },
        "sickLeave": {
            "mode": sick_mode,
            "total": num(sick_total),
            "used": num(b.sick_leave_used),
            "remaining": num(sick_remaining),
        },
        "carryOver": {
            "total": num(b.carry_over_days),
            "used": num(b.carry_over_used),
            "remaining": num(b.carry_over_days - b.carry_over_used),
        },
        "leaveTypeQuotas": quotas,
        "scheduledHolidayDeductions": scheduled,
    }))
}

/// `listMyRequests(userId, query)` (tanpa batas company: semua pengajuan milik user).
pub async fn list_mine(state: &AppState, user_id: i32, q: &MyRequestsQuery) -> ApiResult<Value> {
    let page = q.page.unwrap_or(1);
    let limit = q.limit.unwrap_or(10);
    let skip = page.saturating_sub(1).saturating_mul(limit);
    let year_range = match q.year.filter(|y| *y != 0) {
        Some(y) => Some(iso_year_bounds(y).ok_or_else(invalid_date)?),
        None => None,
    };
    let filters = |qb: &mut sqlx::QueryBuilder<'_, sqlx::Postgres>| {
        qb.push(r#"lr."userId" = "#).push_bind(user_id);
        if let Some(s) = &q.status {
            qb.push(" AND lr.status::text = ").push_bind(s.clone());
        }
        if let Some(t) = q.leave_type_id.filter(|t| *t != 0) {
            qb.push(r#" AND lr."leaveTypeId" = "#).push_bind(t);
        }
        if let Some((from, to)) = year_range {
            qb.push(r#" AND lr."startDate" >= "#)
                .push_bind(from)
                .push(r#" AND lr."startDate" <= "#)
                .push_bind(to);
        }
    };
    let mut count = sqlx::QueryBuilder::new(r#"SELECT COUNT(*) FROM "leaveRequest" lr WHERE "#);
    filters(&mut count);
    let total: i64 = count.build_query_scalar().fetch_one(&state.db).await?;
    let mut items = select_where();
    filters(&mut items);
    items
        .push(r#" ORDER BY lr."createdAt" DESC, lr.id DESC LIMIT "#)
        .push_bind(limit)
        .push(" OFFSET ")
        .push_bind(skip);
    let rows: Vec<RequestRow> = items.build_query_as().fetch_all(&state.db).await?;
    let data: Vec<RequestView> = rows.iter().map(|r| r.view(Include::MOBILE)).collect();
    #[allow(clippy::cast_precision_loss)]
    let pages = (total as f64 / limit as f64).ceil();
    Ok(json!({
        "data": data,
        "meta": { "total": total, "page": page, "limit": limit, "totalPages": num(pages) },
    }))
}

/// `findFirst({ where: { uuid, userId } })`.
async fn find_mine(state: &AppState, user_id: i32, uuid: &str) -> ApiResult<Option<RequestRow>> {
    let uuid = require_db_uuid(uuid)?;
    let mut qb = select_where();
    qb.push("lr.uuid = ")
        .push_bind(uuid)
        .push(r#" AND lr."userId" = "#)
        .push_bind(user_id);
    Ok(qb.build_query_as().fetch_optional(&state.db).await?)
}

/// `getMyRequest(userId, uuid)`.
pub async fn get_mine(state: &AppState, user_id: i32, uuid: &str) -> ApiResult<RequestView> {
    find_mine(state, user_id, uuid)
        .await?
        .map(|r| r.view(Include::MOBILE))
        .ok_or_else(|| ApiError::not_found("Leave request not found"))
}

/// `getMyRequestDocumentUrl(userId, uuid)`.
pub async fn my_document(
    state: &AppState,
    user_id: i32,
    uuid: &str,
) -> ApiResult<mz_files::SignedReadUrl> {
    let row = find_mine(state, user_id, uuid).await?;
    crate::review::signed_document(state, row.and_then(|r| r.document_key)).await
}

/// `reviseLeaveRequest(userId, uuid, dto, document)`.
pub async fn revise(
    state: &AppState,
    user_id: i32,
    company_id: i32,
    uuid: &str,
    dto: &ReviseDto,
    document: Option<&Document>,
) -> ApiResult<RequestView> {
    let existing = find_mine(state, user_id, uuid)
        .await?
        .ok_or_else(|| ApiError::not_found("Leave request not found"))?;
    if existing.status != LeaveStatus::NEEDS_REVISION.as_str() {
        return Err(ApiError::bad_request(
            "Hanya pengajuan berstatus NEEDS_REVISION yang bisa direvisi",
        ));
    }

    // Jenis cuti: konfigurasi baru bila leaveTypeId berubah.
    let mut cfg = existing.leave_type_config();
    let leave_type_id = dto.leave_type_id.or(existing.leave_type_id.map(i64::from));
    if let Some(new_id) = dto.leave_type_id
        && Some(new_id) != existing.leave_type_id.map(i64::from)
    {
        cfg = Some(
            active_leave_type(state, company_id, new_id)
                .await?
                .ok_or_else(|| {
                    ApiError::not_found("Jenis cuti tidak ditemukan atau tidak aktif")
                })?,
        );
    }

    let start_key = dto.start_date.as_deref().map_or_else(
        || mz_leave::day_count::to_date_key(existing.start_date),
        |s| date_key_of(s).to_owned(),
    );
    let end_key = dto.end_date.as_deref().map_or_else(
        || mz_leave::day_count::to_date_key(existing.end_date),
        |s| date_key_of(s).to_owned(),
    );
    let start_day_type = dto
        .start_day_type
        .unwrap_or_else(|| existing.start_day_type());
    let end_day_type = dto.end_day_type.unwrap_or_else(|| existing.end_day_type());
    let (count, _) = count_days(
        state,
        company_id,
        &start_key,
        &end_key,
        start_day_type,
        end_day_type,
    )
    .await?;
    let total_days = count.total_days;
    let start = key_date(&start_key)?;
    let end = key_date(&end_key)?;
    let year = start.year();

    let will_have_document = document.is_some() || existing.document_key.is_some();
    if let Some(c) = &cfg
        && c.requires_document
        && !will_have_document
    {
        return Err(ApiError::bad_request(format!(
            "{} wajib melampirkan dokumen",
            c.name
        )));
    }
    // Pengajuan NEEDS_REVISION tidak ikut terhitung di total PENDING/APPROVED.
    if let Some(c) = &cfg {
        validate_balance(state, user_id, company_id, c, total_days, year).await?;
    }

    match dto.approver_id {
        Opt::Undefined => {}
        Opt::Null => {
            return Err(ApiError::bad_request(
                "Approver tidak valid untuk perusahaan ini",
            ));
        }
        Opt::Value(a) => {
            if !is_valid_approver(state, company_id, a).await? {
                return Err(ApiError::bad_request(
                    "Approver tidak valid untuk perusahaan ini",
                ));
            }
        }
    }

    let document_key = match document {
        Some(doc) => Some(upload_document(state, company_id, user_id, doc).await?),
        None => existing.document_key.clone(),
    };
    let approver = match dto.approver_id.value() {
        Some(a) => Some(i32_of(a)?),
        None => None,
    };
    let leave_type = match leave_type_id {
        Some(t) => Some(i32_of(t)?),
        None => None,
    };

    sqlx::query(
        r#"UPDATE "leaveRequest"
              SET "leaveTypeId" = COALESCE($2, "leaveTypeId"), "approverId" = COALESCE($3, "approverId"),
                  "startDate" = $4, "endDate" = $5,
                  "startDayType" = $6::"LeaveDayType", "endDayType" = $7::"LeaveDayType",
                  "totalDays" = $8, reason = $9, "documentKey" = $10,
                  status = 'PENDING'::"LeaveStatus", "revisionNote" = NULL, "updatedAt" = $11
            WHERE id = $1"#,
    )
    .bind(existing.id)
    .bind(leave_type)
    .bind(approver)
    .bind(start)
    .bind(end)
    .bind(start_day_type.as_str())
    .bind(end_day_type.as_str())
    .bind(total_days)
    .bind(dto.reason.as_deref().unwrap_or(&existing.reason))
    .bind(document_key.as_deref())
    .bind(mz_core::time::now_naive())
    .execute(&state.db)
    .await?;
    let updated = fetch_by_id(&state.db, existing.id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    enqueue_reminder(state, &updated).await;
    Ok(updated.view(Include::MOBILE))
}

/// `cancelRequest(userId, uuid)`: PENDING/NEEDS_REVISION → REJECTED ("Cancelled by employee").
/// Saldo tidak disentuh (hanya pengajuan APPROVED yang pernah memotong saldo).
pub async fn cancel(state: &AppState, user_id: i32, uuid: &str) -> ApiResult<RequestView> {
    let request = find_mine(state, user_id, uuid)
        .await?
        .ok_or_else(|| ApiError::not_found("Leave request not found"))?;
    if request.status != LeaveStatus::PENDING.as_str()
        && request.status != LeaveStatus::NEEDS_REVISION.as_str()
    {
        return Err(ApiError::bad_request(
            "Only pending or needs-revision requests can be cancelled",
        ));
    }
    sqlx::query(
        r#"UPDATE "leaveRequest"
              SET status = 'REJECTED'::"LeaveStatus", "rejectionReason" = 'Cancelled by employee',
                  "updatedAt" = $2
            WHERE id = $1"#,
    )
    .bind(request.id)
    .bind(mz_core::time::now_naive())
    .execute(&state.db)
    .await?;
    let updated = fetch_by_id(&state.db, request.id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    Ok(updated.view(Include::NONE))
}
