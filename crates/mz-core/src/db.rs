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
