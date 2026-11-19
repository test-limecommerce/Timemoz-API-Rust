//! Port `src/divisions/division-membership-hook.ts` (+ `division-locks.ts`).

use mz_audit::{AuditHandle, Lookup, MemberEffect, Model, Row, Touched};
use mz_core::ApiResult;
use mz_core::db::advisory_xact_lock;
use mz_core::time::now_naive;
use serde_json::json;
use sqlx::PgConnection;

/// `lockDivisionsCompany(tx, companyId)`.
pub async fn lock_divisions_company(conn: &mut PgConnection, company_id: i32) -> ApiResult<()> {
    advisory_xact_lock(conn, &format!("divisions:company:{company_id}")).await?;
    Ok(())
}

/// `removeUsersFromDivisionsInTx(tx, userIds, companyId)`: keluarkan karyawan dari SEMUA divisi
/// aktif (soft-delete anggota, naikkan `version` divisi terdampak). `company_id = None` = semua
/// company. WAJIB dipanggil di dalam transaksi pemanggil. Idempotent.
pub async fn remove_users_from_divisions_in_tx(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    user_ids: &[i32],
    company_id: Option<i32>,
) -> ApiResult<()> {
    let mut ids: Vec<i32> = user_ids.iter().copied().filter(|id| *id > 0).collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(());
    }

    // Urutan id company menaik tetap, jadi dua hook tidak bisa saling deadlock.
    let company_ids: Vec<i32> = match company_id {
        Some(c) => vec![c],
        None => sqlx::query_scalar(
            r#"SELECT DISTINCT "companyId" FROM "userCompany" WHERE "userId" = ANY($1) ORDER BY 1"#,
        )
        .bind(&ids)
        .fetch_all(&mut *conn)
        .await?,
    };
    for id in company_ids {
        lock_divisions_company(conn, id).await?;
    }

    let division_ids: Vec<i32> = sqlx::query_scalar(
        r#"SELECT DISTINCT "divisionId" FROM "divisionMember"
           WHERE "userId" = ANY($1) AND "deletedAt" IS NULL AND ($2::int IS NULL OR "companyId" = $2)"#,
    )
    .bind(&ids)
    .bind(company_id)
    .fetch_all(&mut *conn)
    .await?;
    if division_ids.is_empty() {
        return Ok(());
    }

    let now = now_naive();
    // Audit `divisionMember` (keanggotaan: satu baris per divisi), pra-baca lewat pool seperti Node.
    let member_where = {
        let (ids, company_id) = (ids.clone(), company_id);
        Lookup::filter(move |qb| {
            qb.push(r#""userId" = ANY("#).push_bind(ids.clone());
            qb.push(r#") AND "deletedAt" IS NULL"#);
            if let Some(c) = company_id {
                qb.push(r#" AND "companyId" = "#).push_bind(c);
            }
        })
    };
    let members_pre = audit
        .membership_pre(
            Model::DivisionMember,
            MemberEffect::Remove,
            Touched::Where(member_where),
        )
        .await;
    sqlx::query(
        r#"UPDATE "divisionMember" SET "deletedAt" = $3
           WHERE "userId" = ANY($1) AND "deletedAt" IS NULL AND ($2::int IS NULL OR "companyId" = $2)"#,
    )
    .bind(&ids)
    .bind(company_id)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    audit.record_membership(members_pre).await;

    // Naikkan versi divisi terdampak supaya edit dashboard yang basi ditolak 409 DIVISION_STALE.
    // Audit `division` updateMany: `version`/`updatedAt` derau, jadi normalnya tanpa baris.
    let division_where = {
        let division_ids = division_ids.clone();
        Lookup::filter(move |qb| {
            qb.push("id = ANY(").push_bind(division_ids.clone());
            qb.push(r#") AND "deletedAt" IS NULL"#);
        })
    };
    let division_pre = audit.pre_read_many(Model::Division, division_where).await;
    let bumped = sqlx::query(
        r#"UPDATE division SET version = version + 1, "updatedAt" = $2
           WHERE id = ANY($1) AND "deletedAt" IS NULL"#,
    )
    .bind(&division_ids)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    audit
        .record_update_many(
            Model::Division,
            division_pre,
            bumped,
            &Row::new()
                .with("version", json!({ "increment": 1 }))
                .with("updatedAt", json!(mz_core::time::JsDate(now))),
            json!({ "id": { "in": division_ids }, "deletedAt": null }),
        )
        .await;
    Ok(())
}
