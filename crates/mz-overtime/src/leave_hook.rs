//! `overtime-leave.hook.ts` (`OvertimeLeaveHook.cancelForLeave`) + `fullDayDatesOfLeave`
//! (`calendar/day-info.ts`).
//!
//! FR-012: persetujuan cuti sehari penuh membatalkan lembur yang belum selesai pada tanggal
//! itu. Dipanggil di DALAM transaksi persetujuan cuti (Node: `leaves.service.ts` `approve`,
//! sesudah klaim `leaveRequest` dan sebelum `lockBalance`). Tidak mengirim email (pembatalan
//! sistem tidak memberi notifikasi) dan `cancelledById` tetap NULL.
//!
//! Belum ada pemanggil di Rust: modul cuti masih di Node. Crate yang kelak mem-port
//! persetujuan cuti WAJIB memanggil [`cancel_for_leave`] dengan koneksi transaksinya.

use chrono::{NaiveDate, NaiveDateTime};
use sqlx::PgConnection;

use crate::calendar::Leave;
use crate::locks::lock_employee;

const FULL_DAY: &str = "FULL_DAY";

/// `fullDayDatesOfLeave(leave)`: tanggal yang tercakup cuti SEHARI PENUH (R3.5). Setiap
/// tanggal di `[start, end]` termasuk libur mingguan/nasional; ujung awal setengah hari bila
/// `startDayType` bukan `FULL_DAY`, ujung akhir (hanya cuti multi-hari) setengah hari bila
/// `endDayType` bukan `FULL_DAY`. Konsisten dengan [`crate::calendar::leave_coverage`].
///
/// Node melempar `Error` untuk string tanggal tak valid; di sini tanggal sudah bertipe
/// `NaiveDate`. Rentang terbalik (`end < start`) → kosong, sama dengan loop Node.
pub fn full_day_dates_of_leave(leave: &Leave) -> Vec<NaiveDate> {
    leave
        .start_date
        .iter_days()
        .take_while(|d| *d <= leave.end_date)
        .filter(|d| {
            let half = (*d == leave.start_date && leave.start_day_type != FULL_DAY)
                || (*d == leave.end_date
                    && leave.end_date != leave.start_date
                    && leave.end_day_type != FULL_DAY);
            !half
        })
        .collect()
}

/// `OvertimeLeaveHook.cancelForLeave(tx, { companyId, userId, fullDayDates }, now)`.
///
/// `conn` HARUS koneksi transaksi persetujuan cuti: lock karyawan (`ot-emp:`, sama dengan
/// Node dan jalur pengajuan lembur) dilepas saat transaksi selesai, sehingga pengajuan yang
/// berjalan bersamaan melihat cuti ini atau ikut terbatalkan di sini.
///
/// Status `PENDING_APPROVAL`/`PENDING_EMPLOYEE`/`APPROVED` → `CANCELLED`
/// (`EMPLOYEE_ON_LEAVE`); `COMPLETED` sengaja tidak disentuh. `updatedAt` = `now`
/// (Prisma `@updatedAt` pada `updateMany`). `overtimeRequest` bukan model ber-audit.
/// Mengembalikan jumlah baris yang dibatalkan.
pub async fn cancel_for_leave(
    conn: &mut PgConnection,
    company_id: i32,
    user_id: i32,
    full_day_dates: &[NaiveDate],
    now: NaiveDateTime,
) -> sqlx::Result<u64> {
    if full_day_dates.is_empty() {
        return Ok(0);
    }
    lock_employee(&mut *conn, company_id, user_id).await?;
    let done = sqlx::query(
        r#"UPDATE "overtimeRequest"
              SET status = 'CANCELLED', "cancelReason" = 'EMPLOYEE_ON_LEAVE',
                  "cancelledAt" = $4, "updatedAt" = $4
            WHERE "companyId" = $1 AND "userId" = $2
              AND status IN ('PENDING_APPROVAL', 'PENDING_EMPLOYEE', 'APPROVED')
              AND date = ANY($3::date[])"#,
    )
    .bind(company_id)
    .bind(user_id)
    .bind(full_day_dates)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap_or_default()
    }

    fn leave(start: &str, end: &str, sdt: &str, edt: &str) -> Leave {
        Leave {
            start_date: d(start),
            end_date: d(end),
            start_day_type: sdt.into(),
            end_day_type: edt.into(),
        }
    }

    #[test]
    fn full_day_dates_follow_node() {
        // Multi-hari penuh: semua tanggal (termasuk akhir pekan).
        let l = leave("2026-11-13", "2026-11-16", "FULL_DAY", "FULL_DAY");
        assert_eq!(
            full_day_dates_of_leave(&l),
            vec![
                d("2026-11-13"),
                d("2026-11-14"),
                d("2026-11-15"),
                d("2026-11-16")
            ]
        );
        // Ujung awal & akhir setengah hari → hanya tengah.
        let l = leave(
            "2026-11-13",
            "2026-11-15",
            "HALF_DAY_AFTERNOON",
            "HALF_DAY_MORNING",
        );
        assert_eq!(full_day_dates_of_leave(&l), vec![d("2026-11-14")]);
        // Sehari: hanya `startDayType` yang menentukan; `endDayType` diabaikan.
        let l = leave("2026-11-13", "2026-11-13", "FULL_DAY", "HALF_DAY_MORNING");
        assert_eq!(full_day_dates_of_leave(&l), vec![d("2026-11-13")]);
        let l = leave("2026-11-13", "2026-11-13", "HALF_DAY_MORNING", "FULL_DAY");
        assert!(full_day_dates_of_leave(&l).is_empty());
        // Rentang terbalik → kosong.
        let l = leave("2026-11-14", "2026-11-13", "FULL_DAY", "FULL_DAY");
        assert!(full_day_dates_of_leave(&l).is_empty());
    }
}
