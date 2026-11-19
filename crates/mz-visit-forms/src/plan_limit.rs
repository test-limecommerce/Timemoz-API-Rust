//! `assertPlanLimit(tx, companyId, PlanLimit.FORM_TEMPLATES)` (`src/plan-limits/plan-entitlements.ts:226`).
//!
//! Satu implementasi `resolvePlanEntitlements` (bagian cacahan) + `assertPlanLimit` ada di
//! [`mz_sales_geo::plan`]; salinan khusus `maxFormTemplates` di sini dihapus (konsolidasi
//! 2026-11-18: query, kunci advisory, hitungan, dan galat 402 identik).

use mz_core::ApiResult;
use mz_sales_geo::plan::{PlanLimit, assert_plan_limit};
use sqlx::PgConnection;

/// `assertPlanLimit(tx, companyId, FORM_TEMPLATES)` — WAJIB di dalam transaksi yang juga
/// menjalankan `INSERT`-nya (advisory xact lock).
pub async fn assert_form_template_limit(conn: &mut PgConnection, company_id: i32) -> ApiResult<()> {
    assert_plan_limit(conn, company_id, PlanLimit::FormTemplates).await
}
