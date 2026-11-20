//! Persetujuan cuti (`leaves.service.ts:368-620`): `approveRequest` beserta
//! `computeRemaining`, `deductAnnualBalance`, `deductSickBalance`.
//!
//! Urutan transaksi & kunci identik dengan Node:
//! 1. (di luar transaksi) visibilitas + status awal, `company.timeZone`;
//! 2. klaim atomik `leaveRequest` (`UPDATE … WHERE status IN (PENDING, NEEDS_REVISION)`), 0 baris
//!    → 409; baris dibaca ulang SESUDAH klaim (revisi yang mendarat lebih dulu yang disetujui);
//! 3. hook lembur (`ot-emp:` advisory lock + batalkan lembur hari penuh);
//! 4. hitung ulang hari per tahun (libur + pola kerja saat ini); baris saldo tiap tahun dibuat
//!    dulu (naik), baru dikunci naik per tahun;
//! 5. cek tumpang tindih APPROVED di bawah kunci saldo (bebas balapan) → 400 `LEAVE_OVERLAP`;
//! 6. per tahun: potong saldo + riwayat `AUTO_DEDUCTION`, lalu koreksi bawaan Y+1
//!    ([`recompute_next_year_carry_over`]); 7. tulis `balanceBefore/After` (saldo tahun mulai).
//!    Sesudah commit: antrean kalender + email.

use chrono::NaiveDateTime;
use mz_core::{ApiError, ApiResult, AppState};
use mz_leave::balance::{
    BalanceDelta, NewAdjustment, apply_balance_delta, find_balance_by_id, get_or_create_balance_in,
    insert_adjustment, lock_balance,
};
use mz_leave::calendar::{load_holiday_index, off_day_predicate};
use mz_leave::carry_over::recompute_next_year_carry_over;
use mz_leave::day_count::to_date_key;
use mz_leave::enums::{LeaveBalanceChangeSource, LeaveSickMode, LeaveStatus, LeaveType};
use mz_leave::js::num_str;
use mz_leave::math::{
    BalanceAmounts, available, carry_available, requestable, resolve_sick_mode, split_deduction,
};
use mz_leave::models::{LeaveBalance, LeavePolicy};
use mz_leave::policy::{load_policy, policy_carry_over_expired, require_time_zone};
use mz_leave::segments::{
    LeaveRange, OverlapCalendar, count_leave_days_by_year, find_overlapping_leave, overlap_error,
};
use mz_leave::view::get_scheduled_holiday_deductions;
use sqlx::PgConnection;

use crate::model::{Include, RequestRow, RequestView, Visibility, fetch_by_id, select_where};
use crate::notify;
use crate::util::{require_db_uuid, round};

/// Status yang masih bisa diputuskan reviewer (setujui atau tolak).
pub const REVIEWABLE_STATUSES: [LeaveStatus; 2] =
    [LeaveStatus::PENDING, LeaveStatus::NEEDS_REVISION];

/// `leaveAlreadyProcessed()`: 409 klaim status yang kalah dari keputusan bersamaan.
pub fn leave_already_processed() -> ApiError {
    ApiError::coded(
        http::StatusCode::CONFLICT,
        "LEAVE_ALREADY_PROCESSED",
        "Pengajuan cuti sudah diproses",
    )
}

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

/// `deductSickBalance(tx, balance, policy, totalDays, auditCtx)`: hanya sakit LIMITED /
/// UNLIMITED. Sakit DEDUCT_ANNUAL tak pernah sampai sini: persetujuan mengarahkannya ke
/// [`deduct_annual_balance`] supaya mengikuti semua aturan cuti tahunan.
pub async fn deduct_sick_balance(
    conn: &mut PgConnection,
    balance: &LeaveBalance,
    policy: Option<&LeavePolicy>,
    total_days: f64,
    audit: AuditCtx,
) -> ApiResult<()> {
    let mode = resolve_sick_mode(policy);
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
#[allow(clippy::too_many_lines)]
pub async fn approve_request(
    state: &AppState,
    reviewer_id: i32,
    company_id: i32,
    uuid: &str,
    approval_note: Option<String>,
) -> ApiResult<RequestView> {
    // Visibilitas & 400 awal saja; semua angka di bawah berasal dari baris yang dibaca ulang di
    // dalam transaksi sesudah klaim.
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
        return Err(leave_already_processed());
    }

    // Baca ulang sesudah klaim: revisi yang mendarat sebelumnya (tanggal, jenis hari, jenis
    // cuti baru) yang disetujui. Kunci baris dari klaim dipegang sampai commit, dan revisi hanya
    // menulis baris NEEDS_REVISION, jadi baris tak bisa berubah lagi dari sini.
    let current = fetch_by_id(&mut *tx, request.id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    let start_key = to_date_key(current.start_date);
    let end_key = to_date_key(current.end_date);

    // FR-012 (T4): cuti sehari penuh membatalkan lembur yang belum selesai, satu transaksi.
    mz_leave::cancel_overtime_for_leave(
        &mut tx,
        company_id,
        current.user_id,
        current.start_date,
        current.end_date,
        current.start_day_type(),
        current.end_day_type(),
        now,
    )
    .await?;

    // Hitung ulang dengan hari libur & pola kerja saat ini (bisa berubah sejak diajukan).
    let holidays =
        load_holiday_index(&mut tx, company_id, current.start_date, current.end_date).await?;
    let off = off_day_predicate(
        &mut tx,
        company_id,
        current.start_date,
        current.end_date,
        now,
    )
    .await?;
    let is_off = |d| off.is_off(d);
    // Cuti yang melintasi 31 Des / 1 Jan dibebankan ke saldo tiap tahun.
    let (count, segments) = count_leave_days_by_year(
        &start_key,
        &end_key,
        current.start_day_type(),
        current.end_day_type(),
        &holidays.excluded_dates,
        Some(&is_off),
    )?;
    let total_days = count.total_days;
    if total_days == 0.0 {
        return Err(ApiError::coded(
            http::StatusCode::BAD_REQUEST,
            "LEAVE_NO_WORKING_DAYS",
            "Rentang tanggal tidak berisi hari kerja",
        ));
    }

    // Urutan kunci harus sama dengan getOrCreateBalance(Y+1), yang mengambil advisory lock lalu
    // membutuhkan baris Y: pastikan SEMUA baris tahun ada (naik) sebelum kunci baris mana pun,
    // lalu kunci naik. Mengunci Y sebelum membuat Y+1 bisa deadlock (40P01).
    let mut balance_ids = Vec::with_capacity(segments.len());
    for seg in &segments {
        let ensured =
            get_or_create_balance_in(&mut tx, current.user_id, company_id, seg.year).await?;
        balance_ids.push(ensured.id);
    }
    for id in &balance_ids {
        lock_balance(&mut tx, *id).await?;
    }

    // Cek tumpang tindih bebas balapan: persetujuan lain atas cuti yang bertumpuk berbagi
    // minimal satu baris saldo yang dikunci di atas, jadi sudah commit (terlihat di sini) atau
    // menunggu transaksi ini.
    let clash = find_overlapping_leave(
        &mut *tx,
        company_id,
        current.user_id,
        Some(current.id),
        &[LeaveStatus::APPROVED],
        &LeaveRange {
            start: current.start_date,
            end: current.end_date,
            start_day_type: current.start_day_type(),
            end_day_type: current.end_day_type(),
        },
        Some(OverlapCalendar {
            excluded_dates: &holidays.excluded_dates,
            is_weekly_off: &is_off,
        }),
    )
    .await?;
    if let Some(hit) = clash {
        return Err(overlap_error(&hit));
    }

    let policy = load_policy(&mut *tx, company_id).await?;
    let policy_ref = policy.as_ref();

    // Sumber saldo yang dipotong, supaya snapshot sebelum/sesudah mencerminkan saldo itu.
    let config = current.leave_type_config();
    let is_sick_source = match &config {
        Some(cfg) => cfg.balance_source == "SICK",
        None => current.legacy_type() == Some(LeaveType::SICK_LEAVE),
    };
    let source = if is_sick_source {
        Source::Sick
    } else {
        Source::Annual
    };
    let deducts = match &config {
        Some(cfg) => cfg.deduct_balance,
        None => matches!(
            current.legacy_type(),
            Some(LeaveType::ANNUAL_LEAVE | LeaveType::SICK_LEAVE)
        ),
    };
    // Sakit DEDUCT_ANNUAL memakai saldo tahunan, jadi mengikuti semua aturan tahunan: bawaan
    // dulu, hari yang dicadangkan untuk cuti bersama, koreksi bawaan.
    let draws_annual =
        !is_sick_source || resolve_sick_mode(policy_ref) == LeaveSickMode::DEDUCT_ANNUAL;

    let audit = AuditCtx {
        reviewer_id,
        leave_request_id: current.id,
    };
    let mut balance_before = None;
    let mut balance_after = None;

    for (i, (seg, balance_id)) in segments.iter().zip(&balance_ids).enumerate() {
        // Baca baru di bawah kunci: koreksi bawaan tahun sebelumnya bisa mengubah baris ini.
        let balance = find_balance_by_id(&mut *tx, *balance_id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?;
        // Dihitung sesudah lockBalance, di dalam transaksi yang sama.
        let scheduled = if draws_annual {
            get_scheduled_holiday_deductions(
                &mut tx,
                current.user_id,
                company_id,
                seg.year,
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
        let remaining_before =
            compute_remaining(&balance, source, policy_ref, seg.year, &time_zone, now);

        if deducts && seg.days > 0.0 {
            if draws_annual {
                deduct_annual_balance(
                    &mut tx, &balance, policy_ref, seg.days, seg.year, &time_zone, audit,
                    scheduled, now,
                )
                .await?;
                recompute_next_year_carry_over(
                    &mut tx,
                    company_id,
                    current.user_id,
                    seg.year,
                    &time_zone,
                    Some(format!(
                        "Koreksi sisa cuti dibawa: pengajuan #{} tahun {} disetujui",
                        current.id, seg.year
                    )),
                    now,
                )
                .await?;
            } else {
                deduct_sick_balance(&mut tx, &balance, policy_ref, seg.days, audit).await?;
            }
        }

        // balanceBefore/After menggambarkan saldo tahun mulai, seperti sebelum pemecahan.
        if i == 0 {
            balance_before = remaining_before.map(|r| requestable(r, scheduled_f));
            let after = find_balance_by_id(&mut *tx, *balance_id)
                .await?
                .ok_or(sqlx::Error::RowNotFound)?;
            balance_after =
                compute_remaining(&after, source, policy_ref, seg.year, &time_zone, now)
                    .map(|r| requestable(r, scheduled_f));
        }
    }

    sqlx::query(
        r#"UPDATE "leaveRequest"
              SET "rejectionReason" = NULL, "revisionNote" = NULL, "approvalNote" = $2,
                  "totalDays" = $3, "balanceBefore" = $4, "balanceAfter" = $5, "updatedAt" = $6
            WHERE id = $1"#,
    )
    .bind(current.id)
    .bind(approval_note.as_deref())
    .bind(total_days)
    .bind(balance_before)
    .bind(balance_after)
    .bind(mz_core::time::now_naive())
    .execute(&mut *tx)
    .await?;
    let updated = fetch_by_id(&mut *tx, current.id)
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
