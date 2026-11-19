//! Batas paket cacahan baris (`src/plan-limits/plan-entitlements.ts`: `resolvePlanEntitlements`,
//! `assertPlanLimit`, `resolveVisitHistoryDays`; `plan-boost.ts`: `applySalesBoost`).
//!
//! Dipakai geofence (`GEOFENCES`) dan disiapkan untuk toko (`STORES`), formulir kunjungan
//! (`FORM_TEMPLATES`) dan check-in otomatis (`AUTO_CHECKINS`) di tahap berikutnya.
//! Gerbang boolean (`auditLog`, `form`, ...) ada di `mz_core::plan`.

use axum::http::StatusCode;
use mz_core::db::advisory_xact_lock;
use mz_core::{ApiError, ApiResult};
use serde_json::json;
use sqlx::{PgConnection, PgPool};

/// `PlanLimit` Node (nilai = nama kolom `pricingTiers` / `companyPlanOverride`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanLimit {
    Stores,
    Geofences,
    AutoCheckIns,
    FormTemplates,
}

impl PlanLimit {
    /// Nilai enum Node (`maxStores`, `totalGeofence`, ...); juga bagian kunci advisory lock.
    pub const fn column(self) -> &'static str {
        match self {
            Self::Stores => "maxStores",
            Self::Geofences => "totalGeofence",
            Self::AutoCheckIns => "maxAutoCheckIns",
            Self::FormTemplates => "maxFormTemplates",
        }
    }

    /// `LIMIT_ERRORS`: (kode, kata benda).
    const fn error(self) -> (&'static str, &'static str) {
        match self {
            Self::Stores => ("STORE_LIMIT_REACHED", "stores"),
            Self::Geofences => ("GEOFENCE_LIMIT_REACHED", "geofences"),
            Self::AutoCheckIns => ("AUTO_CHECKIN_LIMIT_REACHED", "automatic check-ins"),
            Self::FormTemplates => ("FORM_TEMPLATE_LIMIT_REACHED", "visit forms"),
        }
    }
}

/// Bagian cacahan `PlanEntitlements` sesudah override & boost SALES. `0`/negatif = tanpa batas
/// (kecuali `visit_history_days`: 0 = tanpa batas juga).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanLimits {
    pub max_stores: i32,
    pub total_geofence: i32,
    pub max_auto_check_ins: i32,
    pub max_form_templates: i32,
    pub visit_history_days: i32,
}

impl PlanLimits {
    /// `readPlanLimit`.
    pub fn get(&self, limit: PlanLimit) -> i32 {
        match limit {
            PlanLimit::Stores => self.max_stores,
            PlanLimit::Geofences => self.total_geofence,
            PlanLimit::AutoCheckIns => self.max_auto_check_ins,
            PlanLimit::FormTemplates => self.max_form_templates,
        }
    }
}

/// `SALES_BOOST_LIMITS`.
const BOOST_STORES: i32 = 100;
const BOOST_GEOFENCE: i32 = 20;
const BOOST_AUTO_CHECKINS: i32 = 5000;
const BOOST_FORM_TEMPLATES: i32 = 20;
const BOOST_VISIT_HISTORY_DAYS: i32 = 365;

/// `raiseCount`: 0/negatif = tanpa batas, sudah lebih tinggi dari angka apa pun.
fn raise_count(current: i32, boost: i32) -> i32 {
    if current <= 0 {
        current
    } else {
        current.max(boost)
    }
}

/// `applySalesBoost(tier, boosted)` (bagian cacahan).
pub fn apply_sales_boost(limits: PlanLimits, boosted: bool) -> PlanLimits {
    if !boosted {
        return limits;
    }
    PlanLimits {
        max_stores: raise_count(limits.max_stores, BOOST_STORES),
        total_geofence: raise_count(limits.total_geofence, BOOST_GEOFENCE),
        max_auto_check_ins: raise_count(limits.max_auto_check_ins, BOOST_AUTO_CHECKINS),
        max_form_templates: raise_count(limits.max_form_templates, BOOST_FORM_TEMPLATES),
        visit_history_days: raise_count(limits.visit_history_days, BOOST_VISIT_HISTORY_DAYS),
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    has_tier: bool,
    t_max_stores: Option<i32>,
    t_total_geofence: Option<i32>,
    t_max_auto_check_ins: Option<i32>,
    t_max_form_templates: Option<i32>,
    t_visit_history_days: Option<i32>,
    o_max_stores: Option<i32>,
    o_total_geofence: Option<i32>,
    o_max_auto_check_ins: Option<i32>,
    o_max_form_templates: Option<i32>,
    o_visit_history_days: Option<i32>,
    sales_boost: bool,
    billing_source: Option<String>,
    status: Option<String>,
}

impl Row {
    /// `applyPlanOverride(tier, override)`: kolom override non-null menang.
    fn limits(&self) -> PlanLimits {
        PlanLimits {
            max_stores: self.o_max_stores.or(self.t_max_stores).unwrap_or(0),
            total_geofence: self.o_total_geofence.or(self.t_total_geofence).unwrap_or(0),
            max_auto_check_ins: self
                .o_max_auto_check_ins
                .or(self.t_max_auto_check_ins)
                .unwrap_or(0),
            max_form_templates: self
                .o_max_form_templates
                .or(self.t_max_form_templates)
                .unwrap_or(0),
            visit_history_days: self
                .o_visit_history_days
                .or(self.t_visit_history_days)
                .unwrap_or(0),
        }
    }
}

const SELECT: &str = r#"SELECT pt.id IS NOT NULL AS has_tier,
        pt."maxStores" AS t_max_stores, pt."totalGeofence" AS t_total_geofence,
        pt."maxAutoCheckIns" AS t_max_auto_check_ins, pt."maxFormTemplates" AS t_max_form_templates,
        pt."visitHistoryDays" AS t_visit_history_days,
        po."maxStores" AS o_max_stores, po."totalGeofence" AS o_total_geofence,
        po."maxAutoCheckIns" AS o_max_auto_check_ins, po."maxFormTemplates" AS o_max_form_templates,
        po."visitHistoryDays" AS o_visit_history_days,
        EXISTS (SELECT 1 FROM "companyPlanBoost" pb WHERE pb."companyId" = s."companyId"
                   AND pb.enabled = true AND pb.product::text = 'SALES') AS sales_boost,
        s."billingSource"::text AS billing_source, s.status::text AS status
   FROM subscriptions s
   LEFT JOIN "pricingTiers" pt ON pt.id = s."pricingTierId"
   LEFT JOIN "companyPlanOverride" po ON po."companyId" = s."companyId"
  WHERE s."companyId" = $1"#;

/// `resolvePlanEntitlements` (bagian cacahan). `None` = tanpa paket berjalan = TERBUKA, tanpa batas
/// (langganan ACTIVE/TRIAL terbaru tanpa `pricingTier`, atau tak ada langganan sama sekali; MANUAL
/// INACTIVE terbaru = masa tenggang: paket + override tanpa boost).
pub async fn resolve_plan_limits(
    conn: &mut PgConnection,
    company_id: i32,
) -> sqlx::Result<Option<PlanLimits>> {
    let running: Option<Row> = sqlx::query_as(&format!(
        r#"{SELECT} AND s.status::text IN ('ACTIVE', 'TRIAL')
           ORDER BY s."startDate" DESC, s.id DESC LIMIT 1"#
    ))
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(r) = running {
        return Ok(r
            .has_tier
            .then(|| apply_sales_boost(r.limits(), r.sales_boost)));
    }
    // `resolveLapsedManualEntitlements`.
    let latest: Option<Row> = sqlx::query_as(&format!(
        r#"{SELECT} ORDER BY s."startDate" DESC, s.id DESC LIMIT 1"#
    ))
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(latest
        .filter(|r| {
            r.has_tier
                && r.billing_source.as_deref() == Some("MANUAL")
                && r.status.as_deref() == Some("INACTIVE")
        })
        .map(|r| r.limits()))
}

/// `resolveVisitHistoryDays`: 0 (tanpa paket / tanpa batas) = tanpa batas.
pub async fn resolve_visit_history_days(db: &PgPool, company_id: i32) -> sqlx::Result<i32> {
    let mut conn = db.acquire().await?;
    Ok(resolve_plan_limits(&mut conn, company_id)
        .await?
        .map_or(0, |l| l.visit_history_days))
}

/// `countUsage`: baris yang masih memakai jatah.
async fn count_usage(
    conn: &mut PgConnection,
    company_id: i32,
    limit: PlanLimit,
) -> sqlx::Result<i64> {
    let sql = match limit {
        PlanLimit::Stores => {
            r#"SELECT COUNT(*) FROM store WHERE "companyId" = $1 AND "deletedAt" IS NULL"#
        }
        PlanLimit::Geofences => {
            r#"SELECT COUNT(*) FROM geofence WHERE "companyId" = $1 AND "isActive" = true"#
        }
        PlanLimit::FormTemplates => {
            r#"SELECT COUNT(*) FROM "storeVisitFormTemplate" WHERE "companyId" = $1 AND "deletedAt" IS NULL"#
        }
        PlanLimit::AutoCheckIns => {
            r#"SELECT COUNT(*) FROM "storeVisitLocationEvent" e
                 WHERE e.source = 'ping'
                   AND EXISTS (SELECT 1 FROM "storeVisit" v WHERE v.id = e."visitId" AND v."companyId" = $1)"#
        }
    };
    sqlx::query_scalar(sql)
        .bind(company_id)
        .fetch_one(&mut *conn)
        .await
}

/// `assertPlanLimit(tx, companyId, limit)`: tolak baris ke-(n+1) dengan `402 { code, message,
/// details: { limit, used } }`. WAJIB dipanggil di dalam transaksi yang juga menjalankan INSERT-nya
/// (advisory lock `plan-limit:<kolom>:<companyId>` bertipe `xact`).
pub async fn assert_plan_limit(
    tx: &mut PgConnection,
    company_id: i32,
    limit: PlanLimit,
) -> ApiResult<()> {
    let Some(limits) = resolve_plan_limits(&mut *tx, company_id).await? else {
        return Ok(());
    };
    let max = limits.get(limit);
    if max <= 0 {
        return Ok(());
    }
    advisory_xact_lock(tx, &format!("plan-limit:{}:{company_id}", limit.column())).await?;
    let used = count_usage(tx, company_id, limit).await?;
    if used < i64::from(max) {
        return Ok(());
    }
    let (code, noun) = limit.error();
    Err(ApiError::body(
        StatusCode::PAYMENT_REQUIRED,
        json!({
            "code": code,
            "message": format!("Your plan allows {max} {noun}. Upgrade the plan to add more."),
            "details": { "limit": max, "used": used },
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: PlanLimits = PlanLimits {
        max_stores: 10,
        total_geofence: 2,
        max_auto_check_ins: 0,
        max_form_templates: -1,
        visit_history_days: 7,
    };

    #[test]
    fn boost_raises_counts_but_keeps_unlimited() {
        let b = apply_sales_boost(L, true);
        assert_eq!(b.max_stores, 100);
        assert_eq!(b.total_geofence, 20);
        assert_eq!(b.max_auto_check_ins, 0);
        assert_eq!(b.max_form_templates, -1);
        assert_eq!(b.visit_history_days, 365);
        assert_eq!(apply_sales_boost(L, false), L);
        let big = PlanLimits {
            total_geofence: 500,
            ..L
        };
        assert_eq!(apply_sales_boost(big, true).total_geofence, 500);
    }

    #[test]
    fn limit_columns() {
        assert_eq!(PlanLimit::Geofences.column(), "totalGeofence");
        assert_eq!(L.get(PlanLimit::Stores), 10);
    }
}
