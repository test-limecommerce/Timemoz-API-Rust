//! Pelaksana antrean `overtime` — port `src/overtime/overtime.processor.ts`.
//! Tick berjalan tanpa memeriksa `enabled` (R8). Tiap request punya transaksi pendek
//! sendiri; kegagalan satu request/company dicatat dan tidak menghentikan yang lain.

use chrono::{NaiveDate, NaiveDateTime};
use mz_core::AppState;

use crate::compute::compute_one;
use crate::zone::{local_date_key, ms_of, parse_date_key};

/// `MAX_RECALC_DATES`.
pub const MAX_RECALC_DATES: usize = 31;

#[derive(Debug, Clone, sqlx::FromRow)]
struct TickCompany {
    id: i32,
    time_zone: Option<String>,
}

/// `OVERTIME_TICK_JOB` (`handleTick`), dengan `now` dari pemanggil (test).
pub async fn handle_tick_at(state: &AppState, now: NaiveDateTime) -> anyhow::Result<()> {
    let companies = find_tick_companies(state).await?;
    for company in companies {
        if let Err(e) = tick_company(state, &company, now).await {
            report_failure(&format!("tick gagal untuk company {}", company.id), &e);
        }
    }
    Ok(())
}

/// Fungsi job cron `overtime-tick`.
pub async fn handle_tick(state: AppState) -> anyhow::Result<()> {
    handle_tick_at(&state, mz_core::time::now_naive()).await
}

/// `OVERTIME_RECALC_JOB` (`handleRecalc`): hanya baris COMPLETED yang belum final pada
/// tanggal-tanggal itu (R10); sisanya urusan tick. Payload tidak valid → dibuang + warn.
pub async fn handle_recalc_at(
    state: &AppState,
    company_id: i32,
    user_id: i32,
    date_keys: &[String],
    now: NaiveDateTime,
) -> anyhow::Result<()> {
    let parsed: Option<Vec<NaiveDate>> = date_keys.iter().map(|k| parse_date_key(k)).collect();
    let valid = company_id > 0
        && user_id > 0
        && !date_keys.is_empty()
        && date_keys.len() <= MAX_RECALC_DATES
        && parsed.is_some();
    let Some(mut dates) = parsed.filter(|_| valid) else {
        tracing::warn!("overtime.recalc: payload tidak valid, dibuang");
        return Ok(());
    };
    dates.sort_unstable();
    dates.dedup();
    let rows: Vec<i32> = sqlx::query_scalar(
        r#"SELECT id FROM "overtimeRequest"
           WHERE "companyId" = $1 AND "userId" = $2 AND status = 'COMPLETED'
             AND "finalizedAt" IS NULL AND date = ANY($3)
           ORDER BY date ASC, id ASC"#,
    )
    .bind(company_id)
    .bind(user_id)
    .bind(&dates)
    .fetch_all(&state.db)
    .await?;
    for id in rows {
        compute_isolated(state, id, now).await;
    }
    Ok(())
}

/// Company yang punya baris belum final (satu query). Company tanpa baris itu dilewati,
/// termasuk untuk prune approver (R10).
async fn find_tick_companies(state: &AppState) -> sqlx::Result<Vec<TickCompany>> {
    sqlx::query_as(
        r#"SELECT DISTINCT ON (r."companyId") r."companyId" AS id, c."timeZone" AS time_zone
           FROM "overtimeRequest" r JOIN company c ON c.id = r."companyId"
           WHERE r.status IN ('PENDING_APPROVAL', 'PENDING_EMPLOYEE', 'APPROVED')
              OR (r.status = 'COMPLETED' AND r."finalizedAt" IS NULL)
           ORDER BY r."companyId" ASC"#,
    )
    .fetch_all(&state.db)
    .await
}

/// `findMany({ where: { companyId, ...<$2> }, select: { id }, orderBy: [date, id] })`.
async fn ids<'q, T>(
    state: &AppState,
    sql: &'q str,
    company_id: i32,
    at: T,
) -> sqlx::Result<Vec<i32>>
where
    T: 'q + Send + sqlx::Encode<'q, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    sqlx::query_scalar(sql)
        .bind(company_id)
        .bind(at)
        .fetch_all(&state.db)
        .await
}

async fn tick_company(
    state: &AppState,
    company: &TickCompany,
    now: NaiveDateTime,
) -> anyhow::Result<()> {
    // Langkah 1: sendiri-sendiri; gagal tak menahan 2–5.
    if let Err(e) = crate::prune::prune_approvers(state, company.id).await {
        report_failure(&format!("prune approver company {}", company.id), &e);
    }

    let local_today = local_date_key(ms_of(now), company.time_zone.as_deref());

    // Langkah 2: batalkan yang lewat batas respons.
    let expired = ids(
        state,
        r#"SELECT id FROM "overtimeRequest"
           WHERE "companyId" = $1 AND status IN ('PENDING_APPROVAL', 'PENDING_EMPLOYEE')
             AND "responseDeadline" < $2
           ORDER BY date ASC, id ASC"#,
        company.id,
        now,
    )
    .await?;
    for id in expired {
        cancel_no_response(state, id, now).await;
    }

    // Langkah 3: tanggal sudah lewat → COMPLETED.
    let ended = ids(
        state,
        r#"SELECT id FROM "overtimeRequest"
           WHERE "companyId" = $1 AND status = 'APPROVED' AND date < $2
           ORDER BY date ASC, id ASC"#,
        company.id,
        local_today,
    )
    .await?;
    for id in ended {
        compute_isolated(state, id, now).await;
    }

    // Langkah 4: hitung ulang yang belum final.
    let open = ids(
        state,
        r#"SELECT id FROM "overtimeRequest"
           WHERE "companyId" = $1 AND status = 'COMPLETED' AND "finalizedAt" IS NULL
             AND "finalAt" > $2
           ORDER BY date ASC, id ASC"#,
        company.id,
        now,
    )
    .await?;
    for id in open {
        compute_isolated(state, id, now).await;
    }

    // Langkah 5: finalisasi (computeOne hanya mengisi finalizedAt).
    let due = ids(
        state,
        r#"SELECT id FROM "overtimeRequest"
           WHERE "companyId" = $1 AND status = 'COMPLETED' AND "finalizedAt" IS NULL
             AND "finalAt" <= $2
           ORDER BY date ASC, id ASC"#,
        company.id,
        now,
    )
    .await?;
    for id in due {
        compute_isolated(state, id, now).await;
    }
    Ok(())
}

/// T2/T10: CAS pada status + batas respons; keputusan yang masuk lebih dulu menang.
async fn cancel_no_response(state: &AppState, id: i32, now: NaiveDateTime) {
    let res = sqlx::query(
        r#"UPDATE "overtimeRequest"
           SET status = 'CANCELLED', "cancelReason" = 'NO_RESPONSE', "cancelledAt" = $2,
               "cancelledById" = NULL, "updatedAt" = $3
           WHERE id = $1 AND status IN ('PENDING_APPROVAL', 'PENDING_EMPLOYEE')
             AND "responseDeadline" < $2"#,
    )
    .bind(id)
    .bind(now)
    .bind(mz_core::time::now_naive())
    .execute(&state.db)
    .await;
    if let Err(e) = res {
        report_failure(&format!("batal NO_RESPONSE request {id}"), &e.into());
    }
}

/// Satu transaksi per request; galat dicatat dan ditelan (`guarded`).
pub(crate) async fn compute_isolated(state: &AppState, id: i32, now: NaiveDateTime) {
    let res: anyhow::Result<()> = async {
        let mut tx = state.db.begin().await?;
        compute_one(&mut tx, id, now).await?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        report_failure(&format!("hitung request {id}"), &e);
    }
}

fn report_failure(label: &str, error: &anyhow::Error) {
    tracing::error!("overtime: {label}: {error:#}");
}
