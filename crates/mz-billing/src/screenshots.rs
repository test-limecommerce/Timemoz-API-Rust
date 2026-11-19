//! Port `cleanupCompressedScreenshots` (`src/invoices/invoices.service.ts:2224`, Mudah-API).

use mz_core::AppState;

/// Node: invoices.service.ts:2224 (`cleanupCompressedScreenshots`). Dipanggil saat upgrade paket:
/// hapus file kompresi (GCS) semua screenshot company yang `isCompressed`, lalu kosongkan kolom
/// `isCompressed/compressedFileKey/compressedAt`. Galat hapus per file hanya di-`warn`; galat lain
/// di-`error` dan TIDAK dilempar (upgrade harus tetap jalan). Mengembalikan jumlah baris yang
/// dibersihkan.
pub async fn cleanup_compressed_screenshots(state: &AppState, company_id: i32) -> usize {
    match cleanup(state, company_id).await {
        Ok(n) => n,
        Err(err) => {
            tracing::error!("Error cleaning up compressed screenshots: {err}");
            0
        }
    }
}

async fn cleanup(state: &AppState, company_id: i32) -> anyhow::Result<usize> {
    // Semua screenshot terkompresi milik company: screenshot → timeline → tracking → user →
    // userCompany (status apa pun, `some: { companyId }`).
    let rows: Vec<(i32, String)> = sqlx::query_as(
        r#"SELECT s.id, s."compressedFileKey"
             FROM screenshot s
            WHERE s."isCompressed" = true AND s."compressedFileKey" IS NOT NULL
              AND EXISTS (
                  SELECT 1 FROM "trackingTimeline" tt
                    JOIN tracking t ON t.id = tt."trackingId"
                    JOIN "userCompany" uc ON uc."userId" = t."userId"
                   WHERE tt.id = s."trackingTimelineId" AND uc."companyId" = $1)"#,
    )
    .bind(company_id)
    .fetch_all(&state.db)
    .await?;
    if rows.is_empty() {
        return Ok(0);
    }

    // Delete compressed files from GCS
    for (_, key) in &rows {
        if let Err(err) = mz_files::service::delete_file(state, key).await {
            tracing::warn!("Failed to delete compressed file {key}: {err}");
        }
    }

    // Update DB: clear compressed fields
    let ids: Vec<i32> = rows.iter().map(|(id, _)| *id).collect();
    sqlx::query(
        r#"UPDATE screenshot SET "isCompressed" = false, "compressedFileKey" = NULL, "compressedAt" = NULL
            WHERE id = ANY($1)"#,
    )
    .bind(&ids)
    .execute(&state.db)
    .await?;
    tracing::info!(
        "Cleaned up {} compressed screenshots for company {company_id}",
        rows.len()
    );
    Ok(rows.len())
}
