//! `src/leaves/leave-carry-over.ts`: `recomputeNextYearCarryOver`, menurunkan ulang bawaan
//! Y+1 dan catatan hangus Y dari saldo Y saat ini (menggantikan `correctLateCarryOver`).

use chrono::NaiveDateTime;
use sqlx::PgConnection;

use crate::balance::{
    BalanceDelta, NewAdjustment, apply_balance_delta, find_balance, insert_adjustment, lock_balance,
};
use crate::enums::LeaveBalanceChangeSource;
use crate::js::round2;
use crate::models::LeavePolicy;
use crate::policy::{is_carry_over_expired, load_policy};

/// `RecomputeNextYearCarryOverResult`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CarryOverRecompute {
    pub next_balance_id: i32,
    /// Perubahan `carryOverDays` Y+1 (0 bila tidak berubah).
    pub carry_over_delta: f64,
    /// Hari yang kini tercatat hangus dari Y (baris `forfeit:<id Y>`).
    pub forfeited: f64,
}

/// `recomputeNextYearCarryOver(tx, { companyId, userId, year, timeZone, reason, now })`.
///
/// Panggil di dalam transaksi yang mengubah `annualLeaveUsed`/`annualLeaveTotal` tahun `year`.
/// Mengunci Y lalu Y+1 (tahun naik, urutan kunci semua jalur saldo); mengunci ulang baris yang
/// sudah dipegang transaksi ini tidak berbuat apa-apa. Pemanggil tidak boleh memegang kunci
/// baris tahun setelah Y+1. `None` bila baris Y atau Y+1 tidak ada.
pub async fn recompute_next_year_carry_over(
    conn: &mut PgConnection,
    company_id: i32,
    user_id: i32,
    year: i32,
    time_zone: &str,
    reason: Option<String>,
    now: NaiveDateTime,
) -> sqlx::Result<Option<CarryOverRecompute>> {
    let Some(prev_row) = find_balance(&mut *conn, user_id, company_id, year).await? else {
        return Ok(None);
    };
    let Some(next_row) = find_balance(&mut *conn, user_id, company_id, year + 1).await? else {
        return Ok(None);
    };
    let prev = lock_balance(&mut *conn, prev_row.id).await?;
    let next = lock_balance(&mut *conn, next_row.id).await?;
    // Y+1 dibuat sebelum Y ada: createBalanceInTx tidak menemukan baris Y, jadi tak pernah ada
    // bawaan dari Y. Jangan diberikan belakangan.
    if next.created_at.0 < prev.created_at.0 {
        return Ok(None);
    }

    let policy = load_policy(&mut *conn, company_id).await?;
    let carry_enabled = policy.as_ref().is_some_and(|p| p.carry_over_enabled);
    let max_carry = policy.as_ref().and_then(|p| p.max_carry_over_days);

    let unused = round2(prev.annual_leave_total - prev.annual_leave_used).max(0.0);
    let allowed = if carry_enabled {
        unused.min(max_carry.map_or(f64::INFINITY, f64::from))
    } else {
        0.0
    };

    // Yang sudah diberikan aturan ini: carryOverDays dikurangi setiap perubahan tercatat dari
    // sumber lain (ADJUSTMENT admin, CARRY_OVER_EXPIRY), supaya perubahan itu bertahan.
    let other_changes: Option<f64> = sqlx::query_scalar(
        r#"SELECT SUM(delta) FROM "leaveBalanceAdjustment"
            WHERE "balanceId" = $1 AND field = 'carryOverDays'
              AND source <> 'CARRY_OVER'::"LeaveBalanceChangeSource""#,
    )
    .bind(next.id)
    .fetch_one(&mut *conn)
    .await?;
    let granted = round2(next.carry_over_days - other_changes.unwrap_or(0.0)).max(0.0);

    let mut delta = round2(allowed - granted);
    if delta > 0.0
        && carry_over_expired(
            &mut *conn,
            policy.as_ref(),
            next.id,
            year + 1,
            time_zone,
            now,
        )
        .await?
    {
        // Bawaan yang sudah kedaluwarsa hilang; kenaikan terlambat tidak menghidupkannya.
        delta = 0.0;
    }
    if delta < 0.0 {
        // Jangan menekan carryOverDays di bawah carryOverUsed: hari itu sudah dipakai di Y+1.
        delta = delta.max(round2(next.carry_over_used - next.carry_over_days).min(0.0));
    }

    if delta != 0.0 {
        apply_balance_delta(
            &mut *conn,
            next.id,
            &BalanceDelta {
                carry_over_days: delta,
                ..BalanceDelta::default()
            },
        )
        .await?;
        insert_adjustment(
            &mut *conn,
            &NewAdjustment::system(
                &next,
                "carryOverDays",
                delta,
                reason.unwrap_or_else(|| format!("Koreksi sisa cuti dibawa: saldo {year} berubah")),
                LeaveBalanceChangeSource::CARRY_OVER,
            ),
        )
        .await?;
    }

    // Rumus createBalanceInTx, dengan bawaan yang benar-benar diberikan sekarang.
    let carried = round2(granted + delta);
    let unused_carry = round2(prev.carry_over_days - prev.carry_over_used).max(0.0);
    let forfeited = round2((unused - carried).max(0.0) + unused_carry);
    upsert_forfeit(
        &mut *conn,
        &prev,
        forfeited,
        crate::balance::forfeit_reason(carry_enabled, max_carry),
    )
    .await?;

    Ok(Some(CarryOverRecompute {
        next_balance_id: next.id,
        carry_over_delta: delta,
        forfeited,
    }))
}

/// Cerminan `isCarryOverExpired` (bulan kedaluwarsa) + baris `co-expiry:<id Y+1>`.
async fn carry_over_expired(
    conn: &mut PgConnection,
    policy: Option<&LeavePolicy>,
    next_balance_id: i32,
    next_year: i32,
    time_zone: &str,
    now: NaiveDateTime,
) -> sqlx::Result<bool> {
    if is_carry_over_expired(
        policy.and_then(|p| p.carry_over_expiry_month),
        next_year,
        time_zone,
        now,
    ) {
        return Ok(true);
    }
    let row: Option<i32> =
        sqlx::query_scalar(r#"SELECT id FROM "leaveBalanceAdjustment" WHERE "dedupeKey" = $1"#)
            .bind(format!("co-expiry:{next_balance_id}"))
            .fetch_optional(conn)
            .await?;
    Ok(row.is_some())
}

/// Baris FORFEIT ber-`dedupeKey` unik `forfeit:<id Y>`, jadi dikoreksi di tempat.
async fn upsert_forfeit(
    conn: &mut PgConnection,
    prev: &crate::models::LeaveBalance,
    forfeited: f64,
    reason: String,
) -> sqlx::Result<()> {
    let dedupe_key = format!("forfeit:{}", prev.id);
    let existing: Option<(i32, f64)> =
        sqlx::query_as(r#"SELECT id, delta FROM "leaveBalanceAdjustment" WHERE "dedupeKey" = $1"#)
            .bind(&dedupe_key)
            .fetch_optional(&mut *conn)
            .await?;
    if forfeited <= 0.0 {
        // Tak ada lagi yang hangus; baris "hangus" 0 hari hanya membingungkan riwayat.
        if let Some((id, _)) = existing {
            sqlx::query(r#"DELETE FROM "leaveBalanceAdjustment" WHERE id = $1"#)
                .bind(id)
                .execute(&mut *conn)
                .await?;
        }
        return Ok(());
    }
    if let Some((id, delta)) = existing {
        if round2(delta) != -forfeited {
            sqlx::query(
                r#"UPDATE "leaveBalanceAdjustment" SET delta = $2, reason = $3 WHERE id = $1"#,
            )
            .bind(id)
            .bind(-forfeited)
            .bind(&reason)
            .execute(&mut *conn)
            .await?;
        }
        return Ok(());
    }
    // Seperti createBalanceInTx: total Y TIDAK dikurangi, baris riwayat yang mencatat.
    insert_adjustment(
        conn,
        &NewAdjustment {
            dedupe_key: Some(dedupe_key),
            ..NewAdjustment::system(
                prev,
                "annualLeaveTotal",
                -forfeited,
                reason,
                LeaveBalanceChangeSource::FORFEIT,
            )
        },
    )
    .await
}
