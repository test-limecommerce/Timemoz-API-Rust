//! Port `src/overtime/overtime-locks.ts` (seluruhnya: minggu, karyawan, setelan).
//!
//! Lock bertipe `xact` dengan kunci teks IDENTIK dengan Node
//! (`hashtext('ot-week:' || companyId || ':' || userId || ':' || weekStartKey)`), jadi
//! perhitungan dari Rust dan Node saling mengecualikan saat berjalan berdampingan.
//! WAJIB dipanggil di dalam transaksi (pgbouncer: tak ada lock sesi).

use chrono::NaiveDate;
use sqlx::PgConnection;

use crate::zone::date_key;

/// T8 `lockWeek`: dua perhitungan minggu yang sama (tick ∥ nudge) antre di sini.
pub async fn lock_week(
    conn: &mut PgConnection,
    company_id: i32,
    user_id: i32,
    week_start: NaiveDate,
) -> sqlx::Result<()> {
    sqlx::query(
        r#"SELECT pg_advisory_xact_lock(hashtext('ot-week:' || $1::text || ':' || $2::text || ':' || $3::text))"#,
    )
    .bind(company_id)
    .bind(user_id)
    .bind(date_key(week_start))
    .execute(conn)
    .await?;
    Ok(())
}

/// T113 `tryLockWeek`: versi tanpa menunggu, kunci yang sama persis. `false` = dipegang
/// transaksi lain.
pub async fn try_lock_week(
    conn: &mut PgConnection,
    company_id: i32,
    user_id: i32,
    week_start: NaiveDate,
) -> sqlx::Result<bool> {
    let locked: Option<bool> = sqlx::query_scalar(
        r#"SELECT pg_try_advisory_xact_lock(hashtext('ot-week:' || $1::text || ':' || $2::text || ':' || $3::text)) AS "locked""#,
    )
    .bind(company_id)
    .bind(user_id)
    .bind(date_key(week_start))
    .fetch_one(conn)
    .await?;
    Ok(locked == Some(true))
}

/// T4 `lockEmployee`: pengajuan lembur ∥ persetujuan cuti sehari penuh karyawan yang sama.
/// Kunci IDENTIK Node: `hashtext('ot-emp:' || companyId || ':' || userId)`.
pub async fn lock_employee(
    conn: &mut PgConnection,
    company_id: i32,
    user_id: i32,
) -> sqlx::Result<()> {
    sqlx::query(
        r#"SELECT pg_advisory_xact_lock(hashtext('ot-emp:' || $1::text || ':' || $2::text))"#,
    )
    .bind(company_id)
    .bind(user_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Baris `companyOvertimeSetting` yang dikunci (`OvertimeSettingsLockRow`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::FromRow)]
pub struct OvertimeSettingsLockRow {
    pub enabled: bool,
    #[sqlx(rename = "retroWindowDays")]
    pub retro_window_days: i32,
}

/// T5 (sisi pembuat) `lockSettingsForShare`: create/accept/approve/order membaca setelan
/// dengan `FOR SHARE`. `None` = belum ada baris → pemanggil memakai nilai bawaan (R8).
pub async fn lock_settings_for_share(
    conn: &mut PgConnection,
    company_id: i32,
) -> sqlx::Result<Option<OvertimeSettingsLockRow>> {
    sqlx::query_as(
        r#"SELECT "enabled", "retroWindowDays" FROM "companyOvertimeSetting"
           WHERE "companyId" = $1 FOR SHARE"#,
    )
    .bind(company_id)
    .fetch_optional(conn)
    .await
}

/// T5 (sisi disable) / T6 `lockSettingsForUpdate`: PUT settings mengunci baris dengan
/// `FOR UPDATE` (menunggu pemegang `FOR SHARE`). `None` = belum ada baris.
pub async fn lock_settings_for_update(
    conn: &mut PgConnection,
    company_id: i32,
) -> sqlx::Result<Option<OvertimeSettingsLockRow>> {
    sqlx::query_as(
        r#"SELECT "enabled", "retroWindowDays" FROM "companyOvertimeSetting"
           WHERE "companyId" = $1 FOR UPDATE"#,
    )
    .bind(company_id)
    .fetch_optional(conn)
    .await
}
