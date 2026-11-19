//! Kuota harian reveal, dikonsumsi ATOMIK (`reveal-quota.ts`, §7.4): satu pernyataan
//! `UPDATE ... SET count = count + 1 WHERE <kunci> AND count < :limit` yang membaca dan menulis
//! sekaligus; `0` baris berarti kuota habis ATAU baris hari itu belum lahir (dibedakan dengan
//! `INSERT` yang unik di DB). Maksimal tiga pernyataan, tanpa perulangan.

use mz_core::db::is_unique_violation;
use sqlx::PgPool;

/// §7.4 — 50 reveal per user per kolom per hari.
pub const DAILY_REVEAL_QUOTA: i32 = 50;

/// Kunci pencacah — sama dengan `@@unique` di `schema.prisma`.
#[derive(Debug, Clone)]
pub struct RevealQuotaKey {
    pub company_id: i32,
    pub actor_id: i32,
    pub table_key: String,
    pub column_key: String,
    /// `YYYY-MM-DD` di `company.timeZone`.
    pub day: String,
}

async fn bump(db: &PgPool, key: &RevealQuotaKey, limit: i32) -> sqlx::Result<bool> {
    let res = sqlx::query(
        r#"UPDATE "fieldRevealQuota" SET count = count + 1
            WHERE "companyId" = $1 AND "actorId" = $2 AND "tableKey" = $3
              AND "columnKey" = $4 AND day = $5 AND count < $6"#,
    )
    .bind(key.company_id)
    .bind(key.actor_id)
    .bind(&key.table_key)
    .bind(&key.column_key)
    .bind(&key.day)
    .bind(limit)
    .execute(db)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Menambah satu pemakaian kuota. `true` = boleh lanjut, `false` = kuota habis.
pub async fn consume_reveal_quota(
    db: &PgPool,
    key: &RevealQuotaKey,
    limit: i32,
) -> sqlx::Result<bool> {
    // Batas nol/negatif = tidak boleh sama sekali.
    if limit <= 0 {
        return Ok(false);
    }
    if bump(db, key, limit).await? {
        return Ok(true);
    }
    let created = sqlx::query(
        r#"INSERT INTO "fieldRevealQuota" ("companyId", "actorId", "tableKey", "columnKey", day, count)
           VALUES ($1, $2, $3, $4, $5, 1)"#,
    )
    .bind(key.company_id)
    .bind(key.actor_id)
    .bind(&key.table_key)
    .bind(&key.column_key)
    .bind(&key.day)
    .execute(db)
    .await;
    match created {
        Ok(_) => return Ok(true),
        Err(e) if is_unique_violation(&e) => {}
        Err(e) => return Err(e),
    }
    // Barisnya PASTI ada sekarang; satu `UPDATE` yang sama menjawab sisanya.
    bump(db, key, limit).await
}
