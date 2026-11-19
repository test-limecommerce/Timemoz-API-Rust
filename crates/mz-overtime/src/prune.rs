//! `OvertimeSettingsService.pruneApprovers` (`overtime-settings.service.ts:655`): hapus
//! baris `overtimeApprover` yang penggunanya bukan lagi approver efektif (R6).

use mz_audit::{ActorType, AuditHandle, Lookup, MemberEffect, Model, Touched};
use mz_core::AppState;
use sqlx::{Postgres, QueryBuilder};

use crate::hr::common::settings_svc::effective_approver_user_sql_for;

/// `PRUNE_ACTOR_NAME`.
pub const PRUNE_ACTOR_NAME: &str = "overtime-approver-prune";

/// `NOT effectiveApproverUserWhere(companyId)` untuk baris `"overtimeApprover"` (tanpa
/// alias, kolom ditulis berkualifikasi nama tabel). Filter approver efektif = satu sumber
/// [`crate::hr::common::settings_svc::EFFECTIVE_APPROVER_USER_SQL`]; company-nya dibaca dari kolom baris itu sendiri
/// (`"overtimeApprover"."companyId"`, sudah dikunci `= company_id` di depan).
fn push_stale_filter(qb: &mut QueryBuilder<'_, Postgres>, company_id: i32) {
    qb.push(r#""overtimeApprover"."companyId" = "#)
        .push_bind(company_id)
        .push(r#" AND NOT EXISTS (SELECT 1 FROM users u WHERE u.id = "overtimeApprover"."userId" AND "#)
        .push(effective_approver_user_sql_for(r#""overtimeApprover"."companyId""#))
        .push(")");
}

/// Dijalankan sebagai `system` / `overtime-approver-prune` (audit `overtimeApprover`
/// ditulis langsung, seperti `runAs` Node). Mengembalikan jumlah baris terhapus.
pub async fn prune_approvers(state: &AppState, company_id: i32) -> anyhow::Result<u64> {
    let audit = AuditHandle::run_as(state.db.clone(), ActorType::System, Some(PRUNE_ACTOR_NAME));
    let membership = audit
        .membership_pre(
            Model::OvertimeApprover,
            MemberEffect::Remove,
            Touched::Where(Lookup::filter(move |qb| push_stale_filter(qb, company_id))),
        )
        .await;
    let mut qb = QueryBuilder::<Postgres>::new(r#"DELETE FROM "overtimeApprover" WHERE "#);
    push_stale_filter(&mut qb, company_id);
    let deleted = qb.build().execute(&state.db).await?.rows_affected();
    audit.record_membership(membership).await;
    Ok(deleted)
}
