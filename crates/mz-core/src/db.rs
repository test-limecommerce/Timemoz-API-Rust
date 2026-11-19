//! Helper database bersama.

use sqlx::postgres::PgConnectOptions;
use sqlx::{PgConnection, Postgres, Transaction};

pub type Tx<'a> = Transaction<'a, Postgres>;

/// Opsi koneksi dari `DATABASE_URL`, aman untuk pgbouncer (pool DO, transaction mode).
///
/// sqlx mengirim parameter startup `extra_float_digits=2` secara bawaan, dan
/// pgbouncer menolak koneksi (`unsupported startup parameter`) kecuali
/// parameter itu ada di `ignore_startup_parameters`. Parameter dimatikan:
/// sqlx membaca float dalam format biner, dan sejak Postgres 12 nilai bawaan
/// server (1) sudah menghasilkan teks presisi penuh.
///
/// Cache prepared statement sqlx sengaja TIDAK dimatikan: di transaction mode
/// yang aman hanya pgbouncer >= 1.21 dengan `max_prepared_statements` > 0, dan
/// mematikan cache justru menambah error (sqlx tetap memakai statement bernama).
/// Cek pool dengan `cargo run --release -p mz-core --example pgbouncer_check`.
pub fn connect_options(database_url: &str) -> sqlx::Result<PgConnectOptions> {
    Ok(database_url
        .parse::<PgConnectOptions>()?
        .extra_float_digits(None))
}

/// `prisma/advisory-xact-lock.ts`: `pg_advisory_xact_lock(hashtext(key))`.
/// Varian `xact` wajib karena `DATABASE_URL` produksi memakai pgbouncer
/// transaction pooling.
pub async fn advisory_xact_lock(conn: &mut PgConnection, key: &str) -> sqlx::Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(key)
        .execute(conn)
        .await?;
    Ok(())
}

/// `advisoryXactLock(tx, key, { timeoutMs })`: seperti [`advisory_xact_lock`], tetapi
/// Postgres sendiri membatalkan penantian sesudah `timeout_ms` (SQLSTATE `55P03`, cek
/// dengan [`is_lock_timeout`]) sehingga koneksi langsung kembali ke pool.
/// `set_config(..., true)` = `SET LOCAL`: berlaku sampai transaksi selesai (termasuk
/// lock berikutnya di transaksi yang sama) dan aman dengan pgbouncer transaction pooling.
pub async fn advisory_xact_lock_timeout(
    conn: &mut PgConnection,
    key: &str,
    timeout_ms: u32,
) -> sqlx::Result<()> {
    assert!(timeout_ms > 0, "timeout_ms harus positif");
    sqlx::query("SELECT set_config('lock_timeout', $1, true)")
        .bind(format!("{timeout_ms}ms"))
        .execute(&mut *conn)
        .await?;
    advisory_xact_lock(conn, key).await
}

/// SQLSTATE `55P03` `lock_not_available`: penantian lock melewati `lock_timeout`.
pub fn is_lock_timeout(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(db) if db.code().as_deref() == Some("55P03"))
}

pub async fn advisory_xact_lock_shared(conn: &mut PgConnection, key: &str) -> sqlx::Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock_shared(hashtext($1))")
        .bind(key)
        .execute(conn)
        .await?;
    Ok(())
}

pub fn is_unique_violation(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(db) if db.is_unique_violation())
}

/// Prisma `P2025` (record to update/delete not found) tidak punya padanan di SQL:
/// `UPDATE ... WHERE id = $1` yang tidak mengenai baris hanya mengembalikan 0.
/// Helper ini dipakai di tempat Node memanggil `update({ where: { id } })` dan
/// mengandalkan lemparan P2025 (yang di Node berakhir 500).
pub fn expect_one(affected: u64, what: &str) -> Result<(), crate::ApiError> {
    if affected == 0 {
        return Err(crate::ApiError::internal(anyhow::anyhow!(
            "record to update not found: {what}"
        )));
    }
    Ok(())
}
