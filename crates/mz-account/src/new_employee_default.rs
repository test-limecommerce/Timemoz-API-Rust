//! Port `src/manual-time-settings/new-employee-default.ts`.

use mz_audit::{AuditHandle, MemberEffect, Model, Row, Touched};
use mz_core::ApiResult;
use mz_core::db::advisory_xact_lock_shared;
use sqlx::PgConnection;

/// `applyNewEmployeeManualTimeDefault(tx, { companyId, userId, actorId })`.
///
/// Bila company mematikan `manualTimeAutoApproveNewEmployees`, karyawan baru langsung
/// masuk daftar "Always require approval". Panggil di dalam transaksi yang membuat
/// `userCompany` (invite email: `actor_id` = pengundang; register-invite: `None`).
pub async fn apply_new_employee_manual_time_default(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    company_id: i32,
    user_id: i32,
    actor_id: Option<i32>,
) -> ApiResult<()> {
    // lockManualTimeSettingsShared (T6): join menunggu PUT pengaturan yang berjalan.
    advisory_xact_lock_shared(conn, &format!("manual-time-settings:company:{company_id}")).await?;

    let auto_approve: Option<bool> = sqlx::query_scalar(
        r#"SELECT "manualTimeAutoApproveNewEmployees" FROM company WHERE id = $1"#,
    )
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    // `!company || company.flag` -> tidak melakukan apa pun.
    if auto_approve.unwrap_or(true) {
        return Ok(());
    }

    // Audit keanggotaan `manualTimeApprovalException` (induk `company`). Seperti Node, `data`
    // createMany yang dipakai — terlepas dari `count` hasil `skipDuplicates`.
    let data = Row::new()
        .with("companyId", company_id)
        .with("userId", user_id)
        .with("createdById", actor_id);
    let pre = audit
        .membership_pre(
            Model::ManualTimeApprovalException,
            MemberEffect::Add,
            Touched::Rows(vec![data]),
        )
        .await;
    // skipDuplicates = ON CONFLICT DO NOTHING (partial unique index
    // `manualTimeApprovalException_unique_active`).
    sqlx::query(
        r#"INSERT INTO "manualTimeApprovalException" ("companyId", "userId", "createdById")
           VALUES ($1, $2, $3) ON CONFLICT DO NOTHING"#,
    )
    .bind(company_id)
    .bind(user_id)
    .bind(actor_id)
    .execute(&mut *conn)
    .await?;
    audit.record_membership(pre).await;
    Ok(())
}
