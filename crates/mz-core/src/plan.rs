//! `@RequirePlanFeature` — `PlanFeatureInterceptor` + `PlanEntitlementsService.hasAnyFeature`
//! (`src/plan-limits/*`: `plan-entitlement.enum.ts`, `plan-entitlements.ts`, `plan-boost.ts`).
//!
//! Paket berlaku untuk company user (aturan `userCompanyScope`):
//! - tanpa keanggotaan aktif → terbuka;
//! - langganan berjalan (ACTIVE/TRIAL, `startDate DESC, id DESC`): tanpa `pricingTier` → terbuka;
//!   selain itu `override ?? paket`, lalu boost SALES membuka semua gerbang dan boost TIME
//!   membuka `divisions`;
//! - tanpa langganan berjalan: langganan terbaru MANUAL yang INACTIVE (masa tenggang) =
//!   `override ?? paket` tanpa boost; selain itu terbuka.
//!
//! Node menyimpan hasil 60 dtk per user; di sini dibaca tiap request (perubahan paket
//! berlaku seketika).

use axum::http::StatusCode;
use sqlx::PgPool;

use crate::ApiError;

/// `PlanFeature` (nilai = nama kolom `pricingTiers` / `companyPlanOverride`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanFeature {
    AuditLog,
    Form,
    TimelineRoute,
    Product,
    Order,
    Divisions,
}

impl PlanFeature {
    /// Nama kolom.
    pub const fn column(self) -> &'static str {
        match self {
            Self::AuditLog => "auditLog",
            Self::Form => "form",
            Self::TimelineRoute => "timelineRoute",
            Self::Product => "product",
            Self::Order => "order",
            Self::Divisions => "divisions",
        }
    }

    /// `FEATURE_LABELS` di `plan-feature.interceptor.ts`.
    pub const fn label(self) -> &'static str {
        match self {
            Self::AuditLog => "Audit Log",
            Self::Form => "Form Builder",
            Self::TimelineRoute => "Timeline Route",
            Self::Product => "Product",
            Self::Order => "Order",
            Self::Divisions => "Divisions",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::AuditLog => 0,
            Self::Form => 1,
            Self::TimelineRoute => 2,
            Self::Product => 3,
            Self::Order => 4,
            Self::Divisions => 5,
        }
    }
}

const FEATURES: usize = 6;

/// Gerbang paket satu company sesudah override & boost (`PlanEntitlements`, bagian boolean).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanGates([bool; FEATURES]);

impl PlanGates {
    /// `applyPlanBoosts(applyPlanOverride(tier, override), boosts)`.
    pub fn resolve(
        tier: [bool; FEATURES],
        override_values: [Option<bool>; FEATURES],
        sales_boost: bool,
        time_boost: bool,
    ) -> Self {
        let mut out = [false; FEATURES];
        for (i, slot) in out.iter_mut().enumerate() {
            let t = tier.get(i).copied().unwrap_or(false);
            *slot = override_values.get(i).copied().flatten().unwrap_or(t);
        }
        if sales_boost {
            out = [true; FEATURES];
        }
        if time_boost && let Some(d) = out.get_mut(PlanFeature::Divisions.index()) {
            *d = true;
        }
        Self(out)
    }

    /// `readPlanFeature`.
    pub fn has(self, feature: PlanFeature) -> bool {
        self.0.get(feature.index()).copied().unwrap_or(false)
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    has_tier: bool,
    t_audit_log: Option<bool>,
    t_form: Option<bool>,
    t_timeline_route: Option<bool>,
    t_product: Option<bool>,
    t_order: Option<bool>,
    t_divisions: Option<bool>,
    o_audit_log: Option<bool>,
    o_form: Option<bool>,
    o_timeline_route: Option<bool>,
    o_product: Option<bool>,
    o_order: Option<bool>,
    o_divisions: Option<bool>,
    sales_boost: bool,
    time_boost: bool,
    billing_source: Option<String>,
    status: Option<String>,
}

impl Row {
    fn tier(&self) -> [bool; FEATURES] {
        [
            self.t_audit_log,
            self.t_form,
            self.t_timeline_route,
            self.t_product,
            self.t_order,
            self.t_divisions,
        ]
        .map(|v| v.unwrap_or(false))
    }

    fn override_values(&self) -> [Option<bool>; FEATURES] {
        [
            self.o_audit_log,
            self.o_form,
            self.o_timeline_route,
            self.o_product,
            self.o_order,
            self.o_divisions,
        ]
    }
}

const SELECT: &str = r#"SELECT pt.id IS NOT NULL AS has_tier,
        pt."auditLog" AS t_audit_log, pt.form AS t_form, pt."timelineRoute" AS t_timeline_route,
        pt.product AS t_product, pt."order" AS t_order, pt.divisions AS t_divisions,
        po."auditLog" AS o_audit_log, po.form AS o_form, po."timelineRoute" AS o_timeline_route,
        po.product AS o_product, po."order" AS o_order, po.divisions AS o_divisions,
        EXISTS (SELECT 1 FROM "companyPlanBoost" pb WHERE pb."companyId" = s."companyId"
                   AND pb.enabled = true AND pb.product::text = 'SALES') AS sales_boost,
        EXISTS (SELECT 1 FROM "companyPlanBoost" pb WHERE pb."companyId" = s."companyId"
                   AND pb.enabled = true AND pb.product::text = 'TIME') AS time_boost,
        s."billingSource"::text AS billing_source, s.status::text AS status
   FROM subscriptions s
   LEFT JOIN "pricingTiers" pt ON pt.id = s."pricingTierId"
   LEFT JOIN "companyPlanOverride" po ON po."companyId" = s."companyId"
  WHERE s."companyId" = $1"#;

/// `PlanEntitlementsService.getEntitlements` (bagian gerbang). `None` = tanpa batasan.
pub async fn company_gates(db: &PgPool, company_id: i32) -> sqlx::Result<Option<PlanGates>> {
    let running: Option<Row> = sqlx::query_as(&format!(
        r#"{SELECT} AND s.status::text IN ('ACTIVE', 'TRIAL')
           ORDER BY s."startDate" DESC, s.id DESC LIMIT 1"#
    ))
    .bind(company_id)
    .fetch_optional(db)
    .await?;
    if let Some(r) = running {
        return Ok(r.has_tier.then(|| {
            PlanGates::resolve(r.tier(), r.override_values(), r.sales_boost, r.time_boost)
        }));
    }
    // `resolveLapsedManualEntitlements`: tanpa boost.
    let latest: Option<Row> = sqlx::query_as(&format!(
        r#"{SELECT} ORDER BY s."startDate" DESC, s.id DESC LIMIT 1"#
    ))
    .bind(company_id)
    .fetch_optional(db)
    .await?;
    Ok(latest
        .filter(|r| {
            r.has_tier
                && r.billing_source.as_deref() == Some("MANUAL")
                && r.status.as_deref() == Some("INACTIVE")
        })
        .map(|r| PlanGates::resolve(r.tier(), r.override_values(), false, false)))
}

/// `hasAnyFeature(userId, features)`.
pub async fn has_any_feature(
    db: &PgPool,
    user_id: i32,
    features: &[PlanFeature],
) -> sqlx::Result<bool> {
    let Some(company) = crate::company::find_company_id(db, user_id).await? else {
        return Ok(true);
    };
    Ok(match company_gates(db, company).await? {
        None => true,
        Some(g) => features.iter().any(|f| g.has(*f)),
    })
}

/// `403 PLAN_FEATURE_DISABLED` (label digabung ` / `).
pub fn disabled_error(features: &[PlanFeature]) -> ApiError {
    let labels: Vec<&str> = features.iter().map(|f| f.label()).collect();
    ApiError::coded(
        StatusCode::FORBIDDEN,
        "PLAN_FEATURE_DISABLED",
        format!(
            "{} is not included in this company's plan",
            labels.join(" / ")
        ),
    )
}

/// Node: analytics.service.ts:383 `isPaidTier(tier)` — bukan bernama "Free" (trim, tanpa beda
/// huruf) DAN punya harga. `name = None` = `tier.name` kosong (kolom NOT NULL: pemanggil dengan
/// LEFT JOIN mendapat `None` untuk ketiganya → `false`, sama dengan `!tier`).
pub fn is_paid_tier(
    name: Option<&str>,
    price_per_user_usd: Option<f64>,
    base_price: Option<f64>,
) -> bool {
    if name.unwrap_or_default().trim().to_lowercase() == "free" {
        return false;
    }
    price_per_user_usd.unwrap_or(0.0) > 0.0 || base_price.unwrap_or(0.0) > 0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: [Option<bool>; FEATURES] = [None; FEATURES];

    #[test]
    fn paid_tier_rules() {
        assert!(!is_paid_tier(Some(" Free "), Some(5.0), None));
        assert!(!is_paid_tier(Some("FREE"), Some(5.0), Some(5.0)));
        assert!(is_paid_tier(Some("Pro"), Some(5.0), None));
        assert!(is_paid_tier(Some("Pro"), None, Some(1.0)));
        assert!(!is_paid_tier(Some("Pro"), Some(0.0), Some(0.0)));
        // `tier.name` kosong → hanya harga yang menentukan (Node `(tier.name || '')`).
        assert!(is_paid_tier(None, Some(5.0), None));
        assert!(!is_paid_tier(None, None, None));
    }

    #[test]
    fn override_then_tier_then_boosts() {
        let tier = [true, false, false, false, false, false];
        let g = PlanGates::resolve(tier, NONE, false, false);
        assert!(g.has(PlanFeature::AuditLog));
        assert!(!g.has(PlanFeature::Form));
        let mut ovr = NONE;
        ovr[0] = Some(false);
        ovr[1] = Some(true);
        let g = PlanGates::resolve(tier, ovr, false, false);
        assert!(!g.has(PlanFeature::AuditLog));
        assert!(g.has(PlanFeature::Form));
        // SALES membuka semua walau override menutup.
        let g = PlanGates::resolve(tier, ovr, true, false);
        assert!(g.has(PlanFeature::AuditLog) && g.has(PlanFeature::Divisions));
        // TIME hanya membuka divisions.
        let g = PlanGates::resolve([false; FEATURES], NONE, false, true);
        assert!(g.has(PlanFeature::Divisions));
        assert!(!g.has(PlanFeature::AuditLog));
    }

    #[test]
    fn message_joins_labels() {
        let e = disabled_error(&[PlanFeature::Form, PlanFeature::Order]);
        assert!(matches!(
            e,
            ApiError::Body { status, ref body } if status == StatusCode::FORBIDDEN
                && *body == serde_json::json!({
                    "code": "PLAN_FEATURE_DISABLED",
                    "message": "Form Builder / Order is not included in this company's plan"
                })
        ));
    }
}
