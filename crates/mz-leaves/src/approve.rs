//! Persetujuan cuti (`leaves.service.ts:323-862`): `approveRequest` beserta
//! `computeRemaining`, `deductAnnualBalance`, `deductSickBalance`, `correctLateCarryOver`.
//!
//! Urutan transaksi & kunci identik dengan Node:
//! 1. (di luar transaksi) `getOrCreateBalance` tahun `startDate` + `company.timeZone`;
//! 2. klaim atomik `leaveRequest` (`UPDATE … WHERE status IN (PENDING, NEEDS_REVISION)`), 0 baris → 409;
//! 3. hook lembur (`ot-emp:` advisory lock + batalkan lembur hari penuh);
//! 4. `lockBalance` tahun Y (FOR UPDATE), baca policy, hitung ulang hari (libur + pola kerja);
//! 5. potong saldo + riwayat `AUTO_DEDUCTION`; 6. koreksi bawaan Y+1 (`lockBalance` Y+1 SESUDAH Y);
//! 7. baca ulang saldo, tulis `balanceBefore/After`. Sesudah commit: antrean kalender + email.

use chrono::NaiveDateTime;
use mz_core::{ApiError, ApiResult, AppState};
use mz_leave::balance::{
    BalanceDelta, NewAdjustment, apply_balance_delta, find_balance, find_balance_by_id,
    get_or_create_balance, insert_adjustment, lock_balance,
};
use mz_leave::calendar::{load_holiday_index, off_day_predicate};
use mz_leave::day_count::{count_leave_days, to_date_key};
use mz_leave::enums::{LeaveBalanceChangeSource, LeaveSickMode, LeaveStatus, LeaveType};
use mz_leave::js::num_str;
use mz_leave::math::{
    BalanceAmounts, available, carry_available, requestable, resolve_sick_mode, split_deduction,
};
use mz_leave::models::{LeaveBalance, LeavePolicy};
use mz_leave::policy::{load_policy, local_year, policy_carry_over_expired, require_time_zone};
use mz_leave::view::get_scheduled_holiday_deductions;
use sqlx::PgConnection;

use crate::model::{Include, RequestRow, RequestView, Visibility, fetch_by_id, select_where};
use crate::notify;
use crate::util::{require_db_uuid, round};

/// Sumber saldo yang dipotong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Annual,
    Sick,
}

/// `computeRemaining(b, source, policy, year, timeZone)`: sisa hari untuk sumber itu
/// (`null` = sakit UNLIMITED).
pub fn compute_remaining(
    b: &LeaveBalance,
    source: Source,
    policy: Option<&LeavePolicy>,
    year: i32,
    time_zone: &str,
    now: NaiveDateTime,
) -> Option<f64> {
    if source == Source::Sick {
        match resolve_sick_mode(policy) {
            LeaveSickMode::UNLIMITED => return None,
            LeaveSickMode::LIMITED => return Some(round(b.sick_leave_total - b.sick_leave_used)),
            LeaveSickMode::DEDUCT_ANNUAL => {}
        }
    }
    let expired = policy_carry_over_expired(policy, year, time_zone, now);
    Some(available(&BalanceAmounts::from(b), expired))
}

/// Konteks riwayat (`auditCtx`).
#[derive(Debug, Clone, Copy)]
pub struct AuditCtx {
    pub reviewer_id: i32,
    pub leave_request_id: i32,
}

impl AuditCtx {
    fn reason(self) -> String {
        format!(
            "Potong otomatis: pengajuan cuti #{} disetujui",
            self.leave_request_id
        )
    }

    fn adjustment(self, b: &LeaveBalance, field: &'static str, delta: f64) -> NewAdjustment {
        NewAdjustment {
            adjusted_by_id: Some(self.reviewer_id),
            leave_request_id: Some(self.leave_request_id),
            ..NewAdjustment::system(
                b,
                field,
                delta,
                self.reason(),
                LeaveBalanceChangeSource::AUTO_DEDUCTION,
            )
        }
    }
}

/// `deductAnnualBalance(tx, balance, policy, totalDays, year, timeZone, auditCtx, scheduled)`.
#[allow(clippy::too_many_arguments)]
pub async fn deduct_annual_balance(
    conn: &mut PgConnection,
    balance: &LeaveBalance,
    policy: Option<&LeavePolicy>,
    total_days: f64,
    year: i32,
    time_zone: &str,
    audit: AuditCtx,
    scheduled_holiday_days: usize,
    now: NaiveDateTime,
) -> ApiResult<()> {
    let expired = policy_carry_over_expired(policy, year, time_zone, now);
    let carry_avail = carry_available(&BalanceAmounts::from(balance), expired);
    let annual_avail = balance.annual_leave_total - balance.annual_leave_used;

    // Hari yang dicadangkan untuk cuti bersama tidak boleh dipakai cuti biasa.
    #[allow(clippy::cast_precision_loss)]
    let scheduled = scheduled_holiday_days as f64;
    let left = requestable(annual_avail + carry_avail, scheduled);
    if total_days > left {
        let suffix = if scheduled_holiday_days > 0 {
            format!(" ({scheduled_holiday_days} hari dicadangkan untuk cuti bersama)")
        } else {
            String::new()
        };
        return Err(ApiError::bad_request(format!(
            "Insufficient annual leave balance. Available: {} day(s){suffix}.",
            num_str(left)
        )));
    }

    let split = split_deduction(carry_avail, annual_avail, total_days, false);
    if split.shortfall > 0.0 {
        return Err(ApiError::bad_request(format!(
            "Insufficient annual leave balance. Available: {} day(s).",
            num_str(annual_avail + carry_avail)
        )));
    }

    if split.from_carry > 0.0 {
        apply_balance_delta(
            &mut *conn,
            balance.id,
            &BalanceDelta {
                carry_over_used: split.from_carry,
                ..BalanceDelta::default()
            },
        )
        .await?;
        insert_adjustment(
            &mut *conn,
            &audit.adjustment(balance, "carryOverUsed", split.from_carry),
        )
        .await?;
    }
    if split.from_annual > 0.0 {
        apply_balance_delta(
            &mut *conn,
            balance.id,
            &BalanceDelta {
                annual_leave_used: split.from_annual,
                ..BalanceDelta::default()
            },
        )
        .await?;
        insert_adjustment(
            &mut *conn,
            &audit.adjustment(balance, "annualLeaveUsed", split.from_annual),
        )
        .await?;
    }
    Ok(())
}

/// `deductSickBalance(tx, balance, policy, totalDays, auditCtx)`.
pub async fn deduct_sick_balance(
    conn: &mut PgConnection,
    balance: &LeaveBalance,
    policy: Option<&LeavePolicy>,
    total_days: f64,
    audit: AuditCtx,
) -> ApiResult<()> {
    let mode = resolve_sick_mode(policy);
    if mode == LeaveSickMode::DEDUCT_ANNUAL {
        // Tanpa saldo sakit khusus: potong saldo tahunan.
        let annual_avail = balance.annual_leave_total - balance.annual_leave_used;
        if total_days > annual_avail {
            return Err(ApiError::bad_request(format!(
                "Insufficient leave balance. Available: {} day(s).",
                num_str(annual_avail)
            )));
        }
        apply_balance_delta(
            &mut *conn,
            balance.id,
            &BalanceDelta {
                annual_leave_used: total_days,
                ..BalanceDelta::default()
            },
        )
        .await?;
        insert_adjustment(
            &mut *conn,
            &audit.adjustment(balance, "annualLeaveUsed", total_days),
        )
        .await?;
        return Ok(());
    }
    // UNLIMITED hanya mencatat pemakaian; batas jumlah hanya berlaku untuk LIMITED.
    let sick_avail = balance.sick_leave_total - balance.sick_leave_used;
    if mode == LeaveSickMode::LIMITED && total_days > sick_avail {
        return Err(ApiError::bad_request(format!(
            "Insufficient sick leave balance. Available: {} day(s).",
            num_str(sick_avail)
        )));
    }
    apply_balance_delta(
        &mut *conn,
        balance.id,
        &BalanceDelta {
            sick_leave_used: total_days,
            ..BalanceDelta::default()
        },
    )
    .await?;
    insert_adjustment(
        &mut *conn,
        &audit.adjustment(balance, "sickLeaveUsed", total_days),
    )
    .await?;
    Ok(())
}

/// `correctLateCarryOver(tx, ctx)`: pengajuan tahun lalu yang disetujui sesudah pergantian
/// tahun menurunkan sisa tahun Y, sehingga bawaan Y+1 bisa melebihi batas. Urutan kunci:
/// Y (sudah dipegang pemanggil) lalu Y+1.
#[allow(clippy::too_many_arguments)]
pub async fn correct_late_carry_over(
    conn: &mut PgConnection,
    balance_id: i32,
    user_id: i32,
    company_id: i32,
    year: i32,
    policy: Option<&LeavePolicy>,
    time_zone: &str,
    request_id: i32,
    now: NaiveDateTime,
) -> ApiResult<()> {
    if year >= local_year(now, time_zone) {
        return Ok(());
    }
    let Some(next) = find_balance(&mut *conn, user_id, company_id, year + 1).await? else {
        return Ok(());
    };
    let next_balance = lock_balance(&mut *conn, next.id).await?;
    let prev = find_balance_by_id(&mut *conn, balance_id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;

    let unused = (prev.annual_leave_total - prev.annual_leave_used).max(0.0);
    let allowed = match policy {
        Some(p) if p.carry_over_enabled => {
            unused.min(p.max_carry_over_days.map_or(f64::INFINITY, f64::from))
        }
        _ => 0.0,
    };
    // Jangan mengambil kembali hari bawaan yang sudah terpakai.
    let target = allowed.max(next_balance.carry_over_used);
    let reduce = round(next_balance.carry_over_days - target);
    if reduce <= 0.0 {
        return Ok(());
    }
    apply_balance_delta(
        &mut *conn,
        next_balance.id,
        &BalanceDelta {
            carry_over_days: -reduce,
            ..BalanceDelta::default()
        },
    )
    .await?;
    insert_adjustment(
        &mut *conn,
        &NewAdjustment::system(
            &next_balance,
            "carryOverDays",
            -reduce,
            format!("Koreksi sisa cuti dibawa: pengajuan #{request_id} tahun {year} disetujui"),
            LeaveBalanceChangeSource::CARRY_OVER,
        ),
    )
    .await?;
    Ok(())
}

/// Pengajuan yang terlihat oleh reviewer (`findFirst({ uuid, companyId, ...visibility })`).
pub async fn find_visible(
    state: &AppState,
    reviewer_id: i32,
    company_id: i32,
    uuid: &str,
) -> ApiResult<Option<RequestRow>> {
    let uuid = require_db_uuid(uuid)?;
    let vis = Visibility::load(&state.db, reviewer_id, company_id).await?;
    let mut qb = select_where();
    qb.push(r#"lr.uuid = "#)
        .push_bind(uuid)
        .push(r#" AND lr."companyId" = "#)
        .push_bind(company_id)
        .push(" AND ");
    vis.push(&mut qb, "lr");
    Ok(qb.build_query_as().fetch_optional(&state.db).await?)
}

/// `approveRequest(reviewerId, uuid, dto)`.
pub async fn approve_request(
    state: &AppState,
    reviewer_id: i32,
    company_id: i32,
    uuid: &str,
    approval_note: Option<String>,
) -> ApiResult<RequestView> {
    let request = find_visible(state, reviewer_id, company_id, uuid)
        .await?
        .ok_or_else(|| ApiError::not_found("Leave request not found"))?;
    if request.status != LeaveStatus::PENDING.as_str()
        && request.status != LeaveStatus::NEEDS_REVISION.as_str()
    {
        return Err(ApiError::bad_request(
            "Only pending or needs-revision requests can be approved",
        ));
    }

    // Baris saldo harus ada sebelum transaksi supaya bisa dikunci di dalamnya.
    let start_key = to_date_key(request.start_date);
    let end_key = to_date_key(request.end_date);
    let year = chrono::Datelike::year(&request.start_date);
    let ensured = get_or_create_balance(&state.db, request.user_id, company_id, year).await?;
    // timeZone tersimpan dipakai apa adanya ('UTC' pilihan sah, tanpa fallback Asia/Jakarta).
    let time_zone = require_time_zone(&state.db, company_id).await?;

    let mut tx = state.db.begin().await?;
    let now = mz_core::time::now_naive();
    // Klaim atomik: dari dua persetujuan bersamaan hanya satu yang mendapat 1 baris.
    let claimed = sqlx::query(
        r#"UPDATE "leaveRequest"
              SET status = 'APPROVED'::"LeaveStatus", "reviewedById" = $2, "reviewedAt" = $3,
                  "updatedAt" = $3
            WHERE id = $1 AND status IN ('PENDING'::"LeaveStatus", 'NEEDS_REVISION'::"LeaveStatus")"#,
    )
    .bind(request.id)
    .bind(reviewer_id)
    .bind(now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if claimed == 0 {
        return Err(ApiError::coded(
            http::StatusCode::CONFLICT,
            "LEAVE_ALREADY_PROCESSED",
            "Pengajuan cuti sudah diproses",
        ));
    }

    // FR-012 (T4): cuti sehari penuh membatalkan lembur yang belum selesai, satu transaksi.
    mz_leave::cancel_overtime_for_leave(
        &mut tx,
        company_id,
        request.user_id,
        request.start_date,
        request.end_date,
        request.start_day_type(),
        request.end_day_type(),
        now,
    )
    .await?;

    // Putuskan dari baris terkunci yang dibaca ulang, bukan snapshot sebelum transaksi.
    let balance = lock_balance(&mut tx, ensured.id).await?;
    let policy = load_policy(&mut *tx, company_id).await?;

    // Hitung ulang dengan hari libur & pola kerja saat ini (bisa berubah sejak diajukan).
    let holidays =
        load_holiday_index(&mut tx, company_id, request.start_date, request.end_date).await?;
    let off = off_day_predicate(
        &mut tx,
        company_id,
        request.start_date,
        request.end_date,
        now,
    )
    .await?;
    let is_off = |d| off.is_off(d);
    let total_days = count_leave_days(
        &start_key,
        &end_key,
        request.start_day_type(),
        request.end_day_type(),
        &holidays.excluded_dates,
        Some(&is_off),
    )?
    .total_days;
    if total_days == 0.0 {
        return Err(ApiError::coded(
            http::StatusCode::BAD_REQUEST,
            "LEAVE_NO_WORKING_DAYS",
            "Rentang tanggal tidak berisi hari kerja",
        ));
    }

    let config = request.leave_type_config();
    let source = match &config {
        Some(cfg) if cfg.balance_source == "SICK" => Source::Sick,
        Some(_) => Source::Annual,
        None if request.legacy_type() == Some(LeaveType::SICK_LEAVE) => Source::Sick,
        None => Source::Annual,
    };
    // Dihitung sesudah lockBalance, di dalam transaksi yang sama.
    let scheduled = if source == Source::Annual {
        get_scheduled_holiday_deductions(
            &mut tx,
            request.user_id,
            company_id,
            year,
            &time_zone,
            now,
        )
        .await?
        .len()
    } else {
        0
    };
    #[allow(clippy::cast_precision_loss)]
    let scheduled_f = scheduled as f64;
    let policy_ref = policy.as_ref();
    let balance_before = compute_remaining(&balance, source, policy_ref, year, &time_zone, now)
        .map(|r| requestable(r, scheduled_f));

    let audit = AuditCtx {
        reviewer_id,
        leave_request_id: request.id,
    };
    let deduct = match &config {
        // Sistem baru: konfigurasi companyLeaveType.
        Some(cfg) if cfg.deduct_balance => Some(if cfg.balance_source == "SICK" {
            Source::Sick
        } else {
            Source::Annual
        }),
        Some(_) => None,
        // Lama: enum LeaveType; jenis lain tidak memotong saldo.
        None => match request.legacy_type() {
            Some(LeaveType::ANNUAL_LEAVE) => Some(Source::Annual),
            Some(LeaveType::SICK_LEAVE) => Some(Source::Sick),
            _ => None,
        },
    };
    match deduct {
        Some(Source::Annual) => {
            deduct_annual_balance(
                &mut tx, &balance, policy_ref, total_days, year, &time_zone, audit, scheduled, now,
            )
            .await?;
        }
        Some(Source::Sick) => {
            deduct_sick_balance(&mut tx, &balance, policy_ref, total_days, audit).await?;
        }
        None => {}
    }

    if source == Source::Annual {
        correct_late_carry_over(
            &mut tx,
            balance.id,
            request.user_id,
            company_id,
            year,
            policy_ref,
            &time_zone,
            request.id,
            now,
        )
        .await?;
    }

    let updated_balance = find_balance_by_id(&mut *tx, balance.id).await?;
    let balance_after = match updated_balance {
        Some(b) => compute_remaining(&b, source, policy_ref, year, &time_zone, now)
            .map(|r| requestable(r, scheduled_f)),
        None => balance_before,
    };

    sqlx::query(
        r#"UPDATE "leaveRequest"
              SET "rejectionReason" = NULL, "revisionNote" = NULL, "approvalNote" = $2,
                  "totalDays" = $3, "balanceBefore" = $4, "balanceAfter" = $5, "updatedAt" = $6
            WHERE id = $1"#,
    )
    .bind(request.id)
    .bind(approval_note.as_deref())
    .bind(total_days)
    .bind(balance_before)
    .bind(balance_after)
    .bind(mz_core::time::now_naive())
    .execute(&mut *tx)
    .await?;
    let updated = fetch_by_id(&mut *tx, request.id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    tx.commit().await?;

    // Sesudah commit; enqueue tidak pernah gagal, jadi Google tak bisa menggagalkan persetujuan.
    mz_leave::jobs::enqueue_calendar_sync(
        state,
        updated.id,
        mz_leave::jobs::calendar_approved_job_id(updated.id),
    )
    .await;
    notify::decision(
        state,
        &updated,
        reviewer_id,
        mz_leave::mail::LeaveDecision::Approved {
            approval_note: updated.approval_note.clone(),
        },
    );
    Ok(updated.view(Include::USER_TYPE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mz_core::time::JsDate;

    fn bal(total: f64, used: f64, sick_total: f64, sick_used: f64, carry: f64) -> LeaveBalance {
        LeaveBalance {
            id: 1,
            user_id: 2,
            company_id: 3,
            year: 2026,
            annual_leave_total: total,
            annual_leave_used: used,
            sick_leave_total: sick_total,
            sick_leave_used: sick_used,
            carry_over_days: carry,
            carry_over_used: 0.0,
            accrued_through_month: 0,
            created_at: JsDate(NaiveDateTime::default()),
            updated_at: None,
        }
    }

    #[test]
    fn remaining_per_source() {
        let now = mz_core::time::parse_js_date("2026-06-01T00:00:00Z").unwrap_or_default();
        let b = bal(12.0, 2.5, 5.0, 1.0, 2.0);
        assert_eq!(
            compute_remaining(&b, Source::Annual, None, 2026, "UTC", now),
            Some(11.5)
        );
        // Tanpa policy: sakit DEDUCT_ANNUAL → sisa tahunan.
        assert_eq!(
            compute_remaining(&b, Source::Sick, None, 2026, "UTC", now),
            Some(11.5)
        );
    }

    #[test]
    fn audit_adjustment_shape() {
        let b = bal(12.0, 0.0, 0.0, 0.0, 0.0);
        let a = AuditCtx {
            reviewer_id: 9,
            leave_request_id: 77,
        }
        .adjustment(&b, "annualLeaveUsed", 1.5);
        assert_eq!(a.adjusted_by_id, Some(9));
        assert_eq!(a.leave_request_id, Some(77));
        assert_eq!(a.reason, "Potong otomatis: pengajuan cuti #77 disetujui");
        assert_eq!(a.source, LeaveBalanceChangeSource::AUTO_DEDUCTION);
        assert_eq!(a.dedupe_key, None);
    }
}
