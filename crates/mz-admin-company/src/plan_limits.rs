//! Port `src/plan-limits/{company-plan-limits,plan-boost,plan-boost.service}.ts` yang dipakai
//! endpoint admin-company: batas efektif (tier → paid → override → boost), penulisan kolom
//! TIME `company` (`applyCompanyPlanLimits`), `reapplyPlanOverrideAfterTierSync`, dan
//! `PlanBoostService.setBoost`.
//!
//! "Kolom" diwakili `Map<String, Value>` (urutan kunci = urutan Node) supaya perbandingan
//! `!==` dan bentuk JSONB `previousValues`/`boostedValues` sama persis.

use chrono::NaiveDateTime;
use mz_audit::{AuditHandle, Lookup, Model};
use mz_core::time::now_naive;
use mz_core::{ApiResult, AppState};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use sqlx::types::Json;

pub type Cols = Map<String, Value>;

/// `PAID_TIER_COMPANY_LIMITS`.
pub fn paid_tier_company_limits() -> Cols {
    obj(json!({
        "screenshotResolution": "1920x1080",
        "screenshotRetentionDays": 9999,
        "reportExportsPerMonth": 365,
        "dataHistoryMonths": 365,
    }))
}

pub const TIME_LIMIT_FIELDS: [&str; 7] = [
    "maxProject",
    "screenshotResolution",
    "screenshotRetentionDays",
    "reportExportsPerMonth",
    "dataHistoryMonths",
    "budgetAlertDeliveryChannel",
    "clientProofExportAllowed",
];

pub const SALES_LIMIT_FIELDS: [&str; 11] = [
    "maxStores",
    "totalGeofence",
    "maxAutoCheckIns",
    "maxFormTemplates",
    "visitHistoryDays",
    "auditLog",
    "form",
    "timelineRoute",
    "product",
    "order",
    "divisions",
];

/// `UNLIMITED_BY_LIMIT`.
pub const UNLIMITED_BY_LIMIT: [(&str, i64); 9] = [
    ("maxStores", 0),
    ("totalGeofence", 0),
    ("maxAutoCheckIns", 0),
    ("maxFormTemplates", 0),
    ("visitHistoryDays", 0),
    ("maxProject", 0),
    ("dataHistoryMonths", -1),
    ("reportExportsPerMonth", -1),
    ("screenshotRetentionDays", 36500),
];

pub const UNLIMITED: &str = "UNLIMITED";

pub fn unlimited_of(field: &str) -> Option<i64> {
    UNLIMITED_BY_LIMIT
        .iter()
        .find(|(f, _)| *f == field)
        .map(|(_, v)| *v)
}

pub fn obj(v: Value) -> Cols {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

fn f(m: &Cols, k: &str) -> f64 {
    m.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}

fn i(m: &Cols, k: &str) -> i64 {
    m.get(k).and_then(Value::as_f64).map_or(0, |x| x as i64)
}

/// `===` JS untuk nilai skalar JSON (angka dibandingkan sebagai number).
pub fn js_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (Some(Value::Number(x)), Some(Value::Number(y))) => x.as_f64() == y.as_f64(),
        (None, None) => true,
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// `isPaidTier(tier)`.
pub fn is_paid_tier(tier: Option<&Cols>) -> bool {
    tier.is_some_and(|t| f(t, "basePrice") > 0.0 || f(t, "pricePerUserUsd") > 0.0)
}

/// `resolveOverrideValue`.
pub fn resolve_override_value(field: &str, value: &Value) -> Value {
    if value.as_str() == Some(UNLIMITED) {
        return unlimited_of(field).map_or(Value::Null, Value::from);
    }
    value.clone()
}

/// `computeEffectiveLimits({ tier, isPaidTier, override })` — kunci TIME lalu SALES.
pub fn compute_effective_limits(tier: &Cols, is_paid: bool, ovr: Option<&Cols>) -> Cols {
    let mut out = Map::new();
    for field in TIME_LIMIT_FIELDS.iter().chain(SALES_LIMIT_FIELDS.iter()) {
        out.insert(
            (*field).to_owned(),
            tier.get(*field).cloned().unwrap_or(Value::Null),
        );
    }
    if is_paid {
        for (k, v) in paid_tier_company_limits() {
            out.insert(k, v);
        }
    }
    if let Some(o) = ovr {
        for field in TIME_LIMIT_FIELDS.iter().chain(SALES_LIMIT_FIELDS.iter()) {
            if let Some(v) = o.get(*field).filter(|v| !v.is_null()) {
                out.insert((*field).to_owned(), v.clone());
            }
        }
    }
    out
}

/// `pickTimeLimits`.
pub fn pick_time_limits(limits: &Cols) -> Cols {
    TIME_LIMIT_FIELDS
        .iter()
        .map(|k| {
            (
                (*k).to_owned(),
                limits.get(*k).cloned().unwrap_or(Value::Null),
            )
        })
        .collect()
}

// ── plan-boost.ts ───────────────────────────────────────────────────────────

pub const SALES_BOOST_LIMITS: [(&str, i64); 5] = [
    ("maxStores", 100),
    ("totalGeofence", 20),
    ("maxAutoCheckIns", 5000),
    ("maxFormTemplates", 20),
    ("visitHistoryDays", 365),
];

/// `PLAN_BOOST_COMPANY_SELECT` (urutan kolom).
pub const BOOST_COMPANY_COLUMNS: [&str; 8] = [
    "maxProject",
    "dataHistoryMonths",
    "reportExportsPerMonth",
    "screenshotRetentionDays",
    "screenshotResolution",
    "budgetAlertDeliveryChannel",
    "clientProofExportAllowed",
    "storeVisitCountsAsAttendance",
];

fn raise_count(current: i64, boost: i64) -> i64 {
    if current <= 0 {
        current
    } else {
        current.max(boost)
    }
}

fn raise_signed(current: i64, boost: i64) -> i64 {
    if current < 0 {
        current
    } else {
        current.max(boost)
    }
}

fn raise_retention(current: i64, boost: i64) -> i64 {
    if current <= 0 || current >= 36500 {
        current
    } else {
        current.max(boost)
    }
}

/// `/^(\d{1,5})x(\d{1,5})$/` → piksel, selain itu 0.
fn pixels_of(resolution: &str) -> i64 {
    let Some((w, h)) = resolution.split_once('x') else {
        return 0;
    };
    let ok = |s: &str| !s.is_empty() && s.len() <= 5 && s.bytes().all(|b| b.is_ascii_digit());
    if !ok(w) || !ok(h) {
        return 0;
    }
    w.parse::<i64>().unwrap_or(0) * h.parse::<i64>().unwrap_or(0)
}

/// `applySalesBoost(tier, boosted)`.
pub fn apply_sales_boost(tier: &mut Cols, boosted: bool) {
    if !boosted {
        return;
    }
    for (field, boost) in SALES_BOOST_LIMITS {
        let cur = i(tier, field);
        tier.insert(field.to_owned(), Value::from(raise_count(cur, boost)));
    }
    for gate in [
        "auditLog",
        "divisions",
        "form",
        "timelineRoute",
        "product",
        "order",
    ] {
        tier.insert(gate.to_owned(), Value::Bool(true));
    }
}

/// `applyPlanBoosts(tier, products)`.
pub fn apply_plan_boosts(tier: &Cols, products: &[String]) -> Cols {
    let mut out = tier.clone();
    apply_sales_boost(&mut out, products.iter().any(|p| p == "SALES"));
    if products.iter().any(|p| p == "TIME") {
        out.insert("divisions".to_owned(), Value::Bool(true));
    }
    out
}

/// `boostCompanyColumns(product, current)` — hanya kolom yang benar-benar berubah.
pub fn boost_company_columns(product: &str, current: &Cols) -> Cols {
    let next: Cols = if product == "SALES" {
        obj(json!({ "storeVisitCountsAsAttendance": true }))
    } else {
        let res = current
            .get("screenshotResolution")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let resolution = if pixels_of(res) < pixels_of("1920x1080") {
            "1920x1080".to_owned()
        } else {
            res.to_owned()
        };
        obj(json!({
            "maxProject": raise_count(i(current, "maxProject"), 100),
            "dataHistoryMonths": raise_signed(i(current, "dataHistoryMonths"), 12),
            "reportExportsPerMonth": raise_signed(i(current, "reportExportsPerMonth"), 50),
            "screenshotRetentionDays": raise_retention(i(current, "screenshotRetentionDays"), 30),
            "screenshotResolution": resolution,
            "budgetAlertDeliveryChannel": "EMAIL_SLACK",
            "clientProofExportAllowed": true,
        }))
    };
    next.into_iter()
        .filter(|(k, v)| !js_eq(current.get(k), Some(v)))
        .collect()
}

/// `pickCompanyColumns(current, fields)`.
pub fn pick_company_columns(current: &Cols, fields: &Cols) -> Cols {
    fields
        .keys()
        .map(|k| (k.clone(), current.get(k).cloned().unwrap_or(Value::Null)))
        .collect()
}

/// `restoreCompanyColumns(current, previous, boosted)`.
pub fn restore_company_columns(
    current: &Cols,
    previous: &Cols,
    boosted: &Cols,
) -> (Cols, Vec<String>) {
    let mut restore = Map::new();
    let mut kept = Vec::new();
    for (field, value) in boosted {
        let Some(prev) = previous.get(field) else {
            continue;
        };
        if js_eq(current.get(field), Some(value)) {
            restore.insert(field.clone(), prev.clone());
        } else {
            kept.push(field.clone());
        }
    }
    (restore, kept)
}

/// `parseBoostColumns(product, json)`.
pub fn parse_boost_columns(product: &str, json: Option<&Value>) -> Cols {
    let Some(Value::Object(m)) = json else {
        return Map::new();
    };
    m.iter()
        .filter(|(field, value)| {
            let kind = if product == "SALES" {
                match field.as_str() {
                    "storeVisitCountsAsAttendance" => Some("boolean"),
                    _ => None,
                }
            } else {
                match field.as_str() {
                    "maxProject"
                    | "dataHistoryMonths"
                    | "reportExportsPerMonth"
                    | "screenshotRetentionDays" => Some("number"),
                    "screenshotResolution" => Some("string"),
                    "budgetAlertDeliveryChannel" => Some("channel"),
                    "clientProofExportAllowed" => Some("boolean"),
                    _ => None,
                }
            };
            match kind {
                Some("number") => value.as_f64().is_some_and(|n| n.fract() == 0.0),
                Some("string") => value.is_string(),
                Some("boolean") => value.is_boolean(),
                Some("channel") => {
                    matches!(value.as_str(), Some("INAPP_ONLY" | "EMAIL_SLACK"))
                }
                _ => false,
            }
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// `toPlanBoostStatus(rows)`: `rows` = `[(product, enabled, enabledAt, disabledAt)]`.
pub fn to_plan_boost_status(rows: &[BoostStatusRow]) -> Value {
    let status = |product: &str| {
        let row = rows.iter().find(|r| r.product == product);
        json!({
            "enabled": row.is_some_and(|r| r.enabled),
            "enabledAt": crate::common::date(row.and_then(|r| r.enabled_at)),
            "disabledAt": crate::common::date(row.and_then(|r| r.disabled_at)),
        })
    };
    json!({ "SALES": status("SALES"), "TIME": status("TIME") })
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Deserialize)]
pub struct BoostStatusRow {
    pub product: String,
    pub enabled: bool,
    #[serde(rename = "enabledAt")]
    #[sqlx(rename = "enabledAt")]
    pub enabled_at: Option<NaiveDateTime>,
    #[serde(rename = "disabledAt")]
    #[sqlx(rename = "disabledAt")]
    pub disabled_at: Option<NaiveDateTime>,
}

// ── DB ──────────────────────────────────────────────────────────────────────

/// Kolom `pricingTiers` yang dibaca sebagai objek JSON (`include: { pricingTier: true }`,
/// sebatas yang dipakai).
pub const TIER_JSON: &str = r#"json_build_object(
    'id', pt.id, 'uuid', pt.uuid, 'name', pt.name, 'minUsers', pt."minUsers", 'maxUsers', pt."maxUsers",
    'basePrice', pt."basePrice", 'pricePerUserUsd', pt."pricePerUserUsd",
    'billingInterval', pt."billingInterval", 'productType', pt."productType"::text,
    'paypalPlanId', pt."paypalPlanId", 'status', pt.status,
    'maxProject', pt."maxProject", 'screenshotResolution', pt."screenshotResolution",
    'screenshotRetentionDays', pt."screenshotRetentionDays",
    'reportExportsPerMonth', pt."reportExportsPerMonth", 'dataHistoryMonths', pt."dataHistoryMonths",
    'budgetAlertDeliveryChannel', pt."budgetAlertDeliveryChannel"::text,
    'clientProofExportAllowed', pt."clientProofExportAllowed",
    'maxStores', pt."maxStores", 'totalGeofence', pt."totalGeofence", 'auditLog', pt."auditLog",
    'divisions', pt.divisions, 'form', pt.form, 'timelineRoute', pt."timelineRoute",
    'product', pt.product, 'order', pt."order", 'maxAutoCheckIns', pt."maxAutoCheckIns",
    'maxFormTemplates', pt."maxFormTemplates", 'visitHistoryDays', pt."visitHistoryDays",
    'useIdleDetection', pt."useIdleDetection", 'useCheatDetection', pt."useCheatDetection",
    'useReports', pt."useReports", 'useTimesheetApproval', pt."useTimesheetApproval",
    'useActivityLevel', pt."useActivityLevel", 'useIntegrations', pt."useIntegrations")"#;

/// `findCurrentSubscription` (`startDate desc, id desc`, tanpa filter status, + tier).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CurrentSub {
    pub id: i32,
    pub uuid: uuid::Uuid,
    #[sqlx(rename = "pricingTierId")]
    pub pricing_tier_id: i32,
    #[sqlx(rename = "startDate")]
    pub start_date: NaiveDateTime,
    #[sqlx(rename = "endDate")]
    pub end_date: NaiveDateTime,
    pub status: String,
    #[sqlx(rename = "maxUsers")]
    pub max_users: i32,
    #[sqlx(rename = "pricePerUser")]
    pub price_per_user: f64,
    #[sqlx(rename = "paypalSubscriptionId")]
    pub paypal_subscription_id: Option<String>,
    #[sqlx(rename = "paypalStatus")]
    pub paypal_status: Option<String>,
    #[sqlx(rename = "autoRenew")]
    pub auto_renew: bool,
    #[sqlx(rename = "pendingPricingTierId")]
    pub pending_pricing_tier_id: Option<i32>,
    #[sqlx(rename = "pendingPaypalSubscriptionId")]
    pub pending_paypal_subscription_id: Option<String>,
    #[sqlx(rename = "billingSource")]
    pub billing_source: String,
    #[sqlx(rename = "contractPricePerUser")]
    pub contract_price_per_user: Option<f64>,
    #[sqlx(rename = "contractCurrency")]
    pub contract_currency: Option<String>,
    #[sqlx(rename = "contractPeriodMonths")]
    pub contract_period_months: Option<i32>,
    #[sqlx(rename = "contractTaxPercent")]
    pub contract_tax_percent: f64,
    pub tier: Option<Json<Value>>,
}

impl CurrentSub {
    pub fn tier(&self) -> Option<&Cols> {
        self.tier.as_ref().and_then(|t| t.0.as_object())
    }

    pub fn tier_name(&self) -> Option<&str> {
        self.tier()
            .and_then(|t| t.get("name"))
            .and_then(Value::as_str)
    }

    pub fn is_manual(&self) -> bool {
        self.billing_source == "MANUAL"
    }
}

pub fn current_sub_sql(where_: &str) -> String {
    format!(
        r#"SELECT s.id, s.uuid, s."pricingTierId", s."startDate", s."endDate", s.status::text AS status,
                  s."maxUsers", s."pricePerUser", s."paypalSubscriptionId", s."paypalStatus", s."autoRenew",
                  s."pendingPricingTierId", s."pendingPaypalSubscriptionId",
                  s."billingSource"::text AS "billingSource", s."contractPricePerUser", s."contractCurrency",
                  s."contractPeriodMonths", s."contractTaxPercent",
                  CASE WHEN pt.id IS NULL THEN NULL ELSE {TIER_JSON} END AS tier
           FROM subscriptions s LEFT JOIN "pricingTiers" pt ON pt.id = s."pricingTierId"
           WHERE {where_}"#
    )
}

pub async fn current_sub<'e>(
    exec: impl sqlx::PgExecutor<'e>,
    company_id: i32,
) -> sqlx::Result<Option<CurrentSub>> {
    sqlx::query_as::<_, CurrentSub>(&format!(
        "{} ORDER BY s.\"startDate\" DESC, s.id DESC LIMIT 1",
        current_sub_sql(r#"s."companyId" = $1"#)
    ))
    .bind(company_id)
    .fetch_optional(exec)
    .await
}

/// Kolom batas `companyPlanOverride` (+ `note`), `None` = tidak ada baris.
pub const OVERRIDE_JSON: &str = r#"json_build_object(
    'maxStores', po."maxStores", 'totalGeofence', po."totalGeofence",
    'maxAutoCheckIns', po."maxAutoCheckIns", 'maxFormTemplates', po."maxFormTemplates",
    'visitHistoryDays', po."visitHistoryDays", 'auditLog', po."auditLog", 'form', po.form,
    'timelineRoute', po."timelineRoute", 'product', po.product, 'order', po."order",
    'divisions', po.divisions, 'maxProject', po."maxProject", 'dataHistoryMonths', po."dataHistoryMonths",
    'reportExportsPerMonth', po."reportExportsPerMonth",
    'screenshotRetentionDays', po."screenshotRetentionDays",
    'screenshotResolution', po."screenshotResolution",
    'budgetAlertDeliveryChannel', po."budgetAlertDeliveryChannel"::text,
    'clientProofExportAllowed', po."clientProofExportAllowed", 'note', po.note)"#;

pub async fn override_cols<'e>(
    exec: impl sqlx::PgExecutor<'e>,
    company_id: i32,
) -> sqlx::Result<Option<Cols>> {
    let row: Option<Json<Value>> = sqlx::query_scalar(&format!(
        r#"SELECT {OVERRIDE_JSON} FROM "companyPlanOverride" po WHERE po."companyId" = $1"#
    ))
    .bind(company_id)
    .fetch_optional(exec)
    .await?;
    Ok(row.map(|j| obj(j.0)))
}

/// `PLAN_BOOST_COMPANY_SELECT`.
pub async fn company_boost_cols(
    conn: &mut PgConnection,
    company_id: i32,
) -> sqlx::Result<Option<Cols>> {
    let row: Option<Json<Value>> = sqlx::query_scalar(
        r#"SELECT json_build_object(
              'maxProject', "maxProject", 'dataHistoryMonths', "dataHistoryMonths",
              'reportExportsPerMonth', "reportExportsPerMonth",
              'screenshotRetentionDays', "screenshotRetentionDays",
              'screenshotResolution', "screenshotResolution",
              'budgetAlertDeliveryChannel', "budgetAlertDeliveryChannel"::text,
              'clientProofExportAllowed', "clientProofExportAllowed",
              'storeVisitCountsAsAttendance', "storeVisitCountsAsAttendance")
           FROM company WHERE id = $1"#,
    )
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|j| obj(j.0)))
}

/// Nilai kolom `company` untuk UPDATE dinamis.
#[derive(Debug, Clone)]
pub enum Bind {
    I32(i32),
    Str(String),
    Bool(bool),
    /// `"BudgetAlertDeliveryChannel"`.
    Channel(String),
    Ts(NaiveDateTime),
    OptStr(Option<String>),
    OptI32(Option<i32>),
}

/// Kolom batas (Map) → daftar SET bertipe. Kolom tak dikenal dilewati.
pub fn binds_of(cols: &Cols) -> Vec<(String, Bind)> {
    cols.iter()
        .filter_map(|(k, v)| {
            let b = match k.as_str() {
                "maxProject"
                | "dataHistoryMonths"
                | "reportExportsPerMonth"
                | "screenshotRetentionDays"
                | "maxUsers"
                | "reportExportsUsedThisMonth" => Bind::I32(v.as_f64().map_or(0, |n| n as i32)),
                "screenshotResolution" => Bind::Str(v.as_str().unwrap_or_default().to_owned()),
                "budgetAlertDeliveryChannel" => {
                    Bind::Channel(v.as_str().unwrap_or("INAPP_ONLY").to_owned())
                }
                "clientProofExportAllowed" | "storeVisitCountsAsAttendance" | "isTrial" => {
                    Bind::Bool(v.as_bool().unwrap_or(false))
                }
                _ => return None,
            };
            Some((k.clone(), b))
        })
        .collect()
}

/// `tx.company.update({ where: { id }, data })` tanpa `select` + audit `company.update`.
pub async fn update_company(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    company_id: i32,
    sets: &[(String, Bind)],
) -> ApiResult<()> {
    if sets.is_empty() {
        return Ok(());
    }
    let pre = audit.pre_read(Model::Company, Lookup::Id(company_id)).await;
    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new("UPDATE company SET ");
    for (n, (col, b)) in sets.iter().enumerate() {
        if n > 0 {
            qb.push(", ");
        }
        qb.push(format!("\"{col}\" = "));
        match b {
            Bind::I32(x) => {
                qb.push_bind(*x);
            }
            Bind::Str(s) => {
                qb.push_bind(s.clone());
            }
            Bind::Bool(x) => {
                qb.push_bind(*x);
            }
            Bind::Channel(s) => {
                qb.push_bind(s.clone())
                    .push("::\"BudgetAlertDeliveryChannel\"");
            }
            Bind::Ts(t) => {
                qb.push_bind(*t);
            }
            Bind::OptStr(s) => {
                qb.push_bind(s.clone());
            }
            Bind::OptI32(x) => {
                qb.push_bind(*x);
            }
        }
    }
    qb.push(" WHERE id = ").push_bind(company_id);
    qb.build().execute(&mut *conn).await?;
    if let Some(row) = audit
        .result_row(&mut *conn, Model::Company, Lookup::Id(company_id))
        .await
    {
        audit.record_update(Model::Company, pre, &row).await;
    }
    Ok(())
}

/// Hasil `applyCompanyPlanLimits`.
#[derive(Debug, Clone)]
pub struct Applied {
    pub base: Cols,
    pub written: Cols,
    pub boosted_values: Option<Cols>,
}

/// `applyCompanyPlanLimits(tx, companyId)` — pemanggil WAJIB sudah memegang
/// `SELECT ... FOR UPDATE` baris company di transaksi yang sama.
pub async fn apply_company_plan_limits(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    company_id: i32,
) -> ApiResult<Option<Applied>> {
    let sub = current_sub(&mut *conn, company_id).await?;
    let ovr = override_cols(&mut *conn, company_id).await?;
    let boost: Option<(i32, bool)> = sqlx::query_as(
        r#"SELECT id, enabled FROM "companyPlanBoost" WHERE "companyId" = $1 AND product::text = 'TIME'"#,
    )
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    let company = company_boost_cols(&mut *conn, company_id).await?;

    let (Some(sub), Some(company)) = (sub, company) else {
        return Ok(None);
    };
    let Some(tier) = sub.tier() else {
        return Ok(None);
    };
    let base = pick_time_limits(&compute_effective_limits(
        tier,
        is_paid_tier(Some(tier)),
        ovr.as_ref(),
    ));

    let Some((boost_id, true)) = boost else {
        update_company(&mut *conn, audit, company_id, &binds_of(&base)).await?;
        return Ok(Some(Applied {
            written: base.clone(),
            base,
            boosted_values: None,
        }));
    };

    let mut base_columns = base.clone();
    base_columns.insert(
        "storeVisitCountsAsAttendance".to_owned(),
        company
            .get("storeVisitCountsAsAttendance")
            .cloned()
            .unwrap_or(Value::Bool(false)),
    );
    let boosted = boost_company_columns("TIME", &base_columns);
    let previous = pick_company_columns(&base_columns, &boosted);
    let mut written = base.clone();
    for (k, v) in &boosted {
        written.insert(k.clone(), v.clone());
    }
    update_company(&mut *conn, audit, company_id, &binds_of(&written)).await?;
    sqlx::query(
        r#"UPDATE "companyPlanBoost" SET "previousValues" = $2, "boostedValues" = $3, "updatedAt" = $4
           WHERE id = $1"#,
    )
    .bind(boost_id)
    .bind(Json(Value::Object(previous)))
    .bind(Json(Value::Object(boosted.clone())))
    .bind(now_naive())
    .execute(&mut *conn)
    .await?;
    Ok(Some(Applied {
        base,
        written,
        boosted_values: Some(boosted),
    }))
}

/// `reapplyPlanOverrideAfterTierSync` — tidak pernah gagal; `true` bila kolom ditulis ulang.
pub async fn reapply_plan_override_after_tier_sync(
    state: &AppState,
    audit: &AuditHandle,
    company_id: i32,
) -> bool {
    let run = async {
        let has: Option<i32> =
            sqlx::query_scalar(r#"SELECT id FROM "companyPlanOverride" WHERE "companyId" = $1"#)
                .bind(company_id)
                .fetch_optional(&state.db)
                .await?;
        if has.is_none() {
            return Ok::<bool, mz_core::ApiError>(false);
        }
        let mut tx = state.db.begin().await?;
        sqlx::query(r#"SELECT id FROM company WHERE id = $1 FOR UPDATE"#)
            .bind(company_id)
            .execute(&mut *tx)
            .await?;
        let applied = apply_company_plan_limits(&mut tx, audit, company_id).await?;
        tx.commit().await?;
        Ok(applied.is_some())
    };
    match run.await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(
                company_id,
                error = %err,
                "Gagal memasang ulang override paket company"
            );
            false
        }
    }
}

// ── PlanBoostService.setBoost ───────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct BoostRow {
    enabled: bool,
    #[sqlx(rename = "previousValues")]
    previous_values: Option<Json<Value>>,
    #[sqlx(rename = "boostedValues")]
    boosted_values: Option<Json<Value>>,
    #[sqlx(rename = "enabledAt")]
    enabled_at: Option<NaiveDateTime>,
    #[sqlx(rename = "disabledAt")]
    disabled_at: Option<NaiveDateTime>,
}

/// `PlanBoostService.setBoost`. `Ok(None)` = company tidak ada (`NotFoundException`).
pub async fn set_boost(
    state: &AppState,
    audit: &AuditHandle,
    company_uuid: &uuid::Uuid,
    product: &str,
    enabled: bool,
    operator_id: Option<i32>,
) -> ApiResult<Option<Value>> {
    let mut tx = state.db.begin().await?;
    let Some(company_id) = crate::common::lock_company_by_uuid(&mut tx, company_uuid).await? else {
        return Ok(None);
    };
    let company = company_boost_cols(&mut tx, company_id)
        .await?
        .unwrap_or_default();
    let existing: Option<BoostRow> = sqlx::query_as(
        r#"SELECT enabled, "previousValues", "boostedValues", "enabledAt", "disabledAt"
           FROM "companyPlanBoost" WHERE "companyId" = $1 AND product = $2::"PricingProductType""#,
    )
    .bind(company_id)
    .bind(product)
    .fetch_optional(&mut *tx)
    .await?;

    let base_boosted = parse_boost_columns(
        product,
        existing
            .as_ref()
            .and_then(|e| e.boosted_values.as_ref().map(|j| &j.0)),
    );
    let mut state_obj = obj(json!({
        "companyUuid": company_uuid.to_string(),
        "product": product,
        "enabledAt": crate::common::date(existing.as_ref().and_then(|e| e.enabled_at)),
        "disabledAt": crate::common::date(existing.as_ref().and_then(|e| e.disabled_at)),
        "boostedValues": Value::Object(base_boosted.clone()),
    }));

    if existing.as_ref().is_some_and(|e| e.enabled) == enabled {
        state_obj.insert("enabled".into(), Value::Bool(enabled));
        state_obj.insert("changed".into(), Value::Bool(false));
        tx.commit().await?;
        return Ok(Some(Value::Object(state_obj)));
    }

    let now = now_naive();

    if product == "TIME" {
        let sub = current_sub(&mut *tx, company_id).await?;
        if sub
            .as_ref()
            .is_some_and(|s| s.is_manual() && s.tier.is_some())
        {
            if enabled {
                sqlx::query(
                    r#"INSERT INTO "companyPlanBoost" ("companyId", product, enabled, "enabledAt", "disabledAt",
                           "updatedByOperatorId", "updatedAt")
                       VALUES ($1, 'TIME'::"PricingProductType", true, $2, NULL, $3, $2)
                       ON CONFLICT ("companyId", product) DO UPDATE SET enabled = true, "enabledAt" = $2,
                           "disabledAt" = NULL, "updatedByOperatorId" = $3, "updatedAt" = $2"#,
                )
                .bind(company_id)
                .bind(now)
                .bind(operator_id)
                .execute(&mut *tx)
                .await?;
                let applied = apply_company_plan_limits(&mut tx, audit, company_id).await?;
                state_obj.insert("enabled".into(), Value::Bool(true));
                state_obj.insert("changed".into(), Value::Bool(true));
                state_obj.insert("enabledAt".into(), crate::common::date_of(now));
                state_obj.insert("disabledAt".into(), Value::Null);
                state_obj.insert(
                    "boostedValues".into(),
                    Value::Object(applied.and_then(|a| a.boosted_values).unwrap_or_default()),
                );
            } else {
                disable_boost_row(&mut tx, company_id, product, now, operator_id).await?;
                let applied = apply_company_plan_limits(&mut tx, audit, company_id).await?;
                let restored = applied.map_or_else(Map::new, |a| {
                    let mut w = a.written;
                    w.insert(
                        "storeVisitCountsAsAttendance".into(),
                        company
                            .get("storeVisitCountsAsAttendance")
                            .cloned()
                            .unwrap_or(Value::Null),
                    );
                    pick_company_columns(&w, &base_boosted)
                });
                state_obj.insert("enabled".into(), Value::Bool(false));
                state_obj.insert("changed".into(), Value::Bool(true));
                state_obj.insert("disabledAt".into(), crate::common::date_of(now));
                state_obj.insert("restoredValues".into(), Value::Object(restored));
                state_obj.insert("keptFields".into(), json!([]));
            }
            tx.commit().await?;
            return Ok(Some(Value::Object(state_obj)));
        }
    }

    if enabled {
        let boosted = boost_company_columns(product, &company);
        let previous = pick_company_columns(&company, &boosted);
        update_company(&mut tx, audit, company_id, &binds_of(&boosted)).await?;
        sqlx::query(
            r#"INSERT INTO "companyPlanBoost" ("companyId", product, enabled, "previousValues", "boostedValues",
                   "enabledAt", "disabledAt", "updatedByOperatorId", "updatedAt")
               VALUES ($1, $2::"PricingProductType", true, $3, $4, $5, NULL, $6, $5)
               ON CONFLICT ("companyId", product) DO UPDATE SET enabled = true, "previousValues" = $3,
                   "boostedValues" = $4, "enabledAt" = $5, "disabledAt" = NULL,
                   "updatedByOperatorId" = $6, "updatedAt" = $5"#,
        )
        .bind(company_id)
        .bind(product)
        .bind(Json(Value::Object(previous)))
        .bind(Json(Value::Object(boosted.clone())))
        .bind(now)
        .bind(operator_id)
        .execute(&mut *tx)
        .await?;
        state_obj.insert("enabled".into(), Value::Bool(true));
        state_obj.insert("changed".into(), Value::Bool(true));
        state_obj.insert("enabledAt".into(), crate::common::date_of(now));
        state_obj.insert("disabledAt".into(), Value::Null);
        state_obj.insert("boostedValues".into(), Value::Object(boosted));
        tx.commit().await?;
        return Ok(Some(Value::Object(state_obj)));
    }

    let previous = parse_boost_columns(
        product,
        existing
            .as_ref()
            .and_then(|e| e.previous_values.as_ref().map(|j| &j.0)),
    );
    let (restore, kept) = restore_company_columns(&company, &previous, &base_boosted);
    update_company(&mut tx, audit, company_id, &binds_of(&restore)).await?;
    disable_boost_row(&mut tx, company_id, product, now, operator_id).await?;
    state_obj.insert("enabled".into(), Value::Bool(false));
    state_obj.insert("changed".into(), Value::Bool(true));
    state_obj.insert("disabledAt".into(), crate::common::date_of(now));
    state_obj.insert("restoredValues".into(), Value::Object(restore));
    state_obj.insert("keptFields".into(), json!(kept));
    tx.commit().await?;
    Ok(Some(Value::Object(state_obj)))
}

async fn disable_boost_row(
    conn: &mut PgConnection,
    company_id: i32,
    product: &str,
    now: NaiveDateTime,
    operator_id: Option<i32>,
) -> ApiResult<()> {
    let n = sqlx::query(
        r#"UPDATE "companyPlanBoost" SET enabled = false, "disabledAt" = $3, "updatedByOperatorId" = $4,
               "updatedAt" = $3
           WHERE "companyId" = $1 AND product = $2::"PricingProductType""#,
    )
    .bind(company_id)
    .bind(product)
    .bind(now)
    .bind(operator_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    // Prisma `update` tanpa baris = P2025 (500 di sini; tidak terjangkau: dicek `enabled` dulu).
    mz_core::db::expect_one(n, "companyPlanBoost")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn free_company() -> Cols {
        obj(json!({
            "maxProject": 3, "dataHistoryMonths": 3, "reportExportsPerMonth": 3,
            "screenshotRetentionDays": 7, "screenshotResolution": "640x360",
            "budgetAlertDeliveryChannel": "INAPP_ONLY", "clientProofExportAllowed": false,
            "storeVisitCountsAsAttendance": false
        }))
    }

    #[test]
    fn is_paid_tier_by_either_price() {
        assert!(!is_paid_tier(None));
        assert!(!is_paid_tier(Some(&obj(
            json!({"basePrice": 0, "pricePerUserUsd": 0})
        ))));
        assert!(is_paid_tier(Some(&obj(
            json!({"basePrice": 0, "pricePerUserUsd": 4.5})
        ))));
        assert!(is_paid_tier(Some(&obj(
            json!({"basePrice": 10, "pricePerUserUsd": 0})
        ))));
    }

    #[test]
    fn effective_limits_tier_then_paid_then_override() {
        let tier = obj(json!({
            "id": 9, "basePrice": 10, "maxProject": 5, "screenshotResolution": "640x360",
            "screenshotRetentionDays": 7, "reportExportsPerMonth": 3, "dataHistoryMonths": 3,
            "budgetAlertDeliveryChannel": "INAPP_ONLY", "clientProofExportAllowed": false,
            "maxStores": 10, "totalGeofence": 2, "maxAutoCheckIns": 0, "maxFormTemplates": 0,
            "visitHistoryDays": 0, "auditLog": false, "form": false, "timelineRoute": false,
            "product": false, "order": false, "divisions": false
        }));
        let ovr =
            obj(json!({ "maxProject": 0, "auditLog": false, "maxStores": null, "note": "x" }));
        let out = compute_effective_limits(&tier, true, Some(&ovr));
        assert_eq!(out.get("screenshotRetentionDays"), Some(&json!(9999)));
        assert_eq!(out.get("dataHistoryMonths"), Some(&json!(365)));
        // 0/false di override tetap menang.
        assert_eq!(out.get("maxProject"), Some(&json!(0)));
        assert_eq!(out.get("maxStores"), Some(&json!(10)));
        assert!(!out.contains_key("id") && !out.contains_key("note"));
        assert_eq!(out.keys().next().map(String::as_str), Some("maxProject"));
    }

    #[test]
    fn resolve_unlimited_by_column() {
        assert_eq!(
            resolve_override_value("maxStores", &json!("UNLIMITED")),
            json!(0)
        );
        assert_eq!(
            resolve_override_value("dataHistoryMonths", &json!("UNLIMITED")),
            json!(-1)
        );
        assert_eq!(
            resolve_override_value("screenshotRetentionDays", &json!("UNLIMITED")),
            json!(36500)
        );
        assert_eq!(resolve_override_value("maxStores", &json!(12)), json!(12));
        assert_eq!(
            resolve_override_value("maxStores", &Value::Null),
            Value::Null
        );
    }

    #[test]
    fn time_boost_raises_free_columns() {
        let b = boost_company_columns("TIME", &free_company());
        assert_eq!(
            Value::Object(b),
            json!({
                "maxProject": 100, "dataHistoryMonths": 12, "reportExportsPerMonth": 50,
                "screenshotRetentionDays": 30, "screenshotResolution": "1920x1080",
                "budgetAlertDeliveryChannel": "EMAIL_SLACK", "clientProofExportAllowed": true
            })
        );
    }

    #[test]
    fn time_boost_skips_higher_or_unlimited() {
        let mut c = free_company();
        c.insert("maxProject".into(), json!(0));
        c.insert("dataHistoryMonths".into(), json!(-1));
        c.insert("reportExportsPerMonth".into(), json!(0));
        c.insert("screenshotRetentionDays".into(), json!(36500));
        c.insert("screenshotResolution".into(), json!("2560x1440"));
        c.insert("budgetAlertDeliveryChannel".into(), json!("EMAIL_SLACK"));
        let b = boost_company_columns("TIME", &c);
        // 0 pada kolom ber-sentinel -1 tetap dinaikkan.
        assert_eq!(
            Value::Object(b),
            json!({ "reportExportsPerMonth": 50, "clientProofExportAllowed": true })
        );
    }

    #[test]
    fn sales_boost_only_attendance() {
        let b = boost_company_columns("SALES", &free_company());
        assert_eq!(
            Value::Object(b),
            json!({ "storeVisitCountsAsAttendance": true })
        );
        let mut c = free_company();
        c.insert("storeVisitCountsAsAttendance".into(), json!(true));
        assert!(boost_company_columns("SALES", &c).is_empty());
    }

    #[test]
    fn restore_keeps_changed_columns() {
        let current = obj(json!({ "maxProject": 100, "dataHistoryMonths": 24 }));
        let previous = obj(json!({ "maxProject": 3, "dataHistoryMonths": 3 }));
        let boosted = obj(json!({ "maxProject": 100, "dataHistoryMonths": 12 }));
        let (restore, kept) = restore_company_columns(&current, &previous, &boosted);
        assert_eq!(Value::Object(restore), json!({ "maxProject": 3 }));
        assert_eq!(kept, vec!["dataHistoryMonths".to_owned()]);
    }

    #[test]
    fn parse_boost_columns_filters_product_and_types() {
        let raw = json!({
            "maxProject": 100, "storeVisitCountsAsAttendance": true, "foo": 1,
            "dataHistoryMonths": "12", "budgetAlertDeliveryChannel": "SMS",
            "screenshotResolution": "1920x1080"
        });
        assert_eq!(
            Value::Object(parse_boost_columns("TIME", Some(&raw))),
            json!({ "maxProject": 100, "screenshotResolution": "1920x1080" })
        );
        assert_eq!(
            Value::Object(parse_boost_columns("SALES", Some(&raw))),
            json!({ "storeVisitCountsAsAttendance": true })
        );
        assert!(parse_boost_columns("TIME", Some(&json!([1]))).is_empty());
        assert!(parse_boost_columns("TIME", None).is_empty());
    }

    #[test]
    fn plan_boosts_sales_and_time() {
        let tier = obj(json!({
            "maxStores": 10, "totalGeofence": 0, "maxAutoCheckIns": 9000, "maxFormTemplates": 1,
            "visitHistoryDays": 7, "auditLog": false, "divisions": false, "form": false,
            "timelineRoute": false, "product": false, "order": false, "uuid": "u"
        }));
        let t = apply_plan_boosts(&tier, &["TIME".into()]);
        assert_eq!(t.get("divisions"), Some(&json!(true)));
        assert_eq!(t.get("form"), Some(&json!(false)));
        let s = apply_plan_boosts(&tier, &["SALES".into()]);
        assert_eq!(s.get("maxStores"), Some(&json!(100)));
        assert_eq!(s.get("totalGeofence"), Some(&json!(0)));
        assert_eq!(s.get("maxAutoCheckIns"), Some(&json!(9000)));
        assert_eq!(s.get("uuid"), Some(&json!("u")));
        assert_eq!(s.get("order"), Some(&json!(true)));
    }

    #[test]
    fn boost_status_always_has_both_products() {
        let v = to_plan_boost_status(&[]);
        assert_eq!(
            v,
            json!({
                "SALES": { "enabled": false, "enabledAt": null, "disabledAt": null },
                "TIME": { "enabled": false, "enabledAt": null, "disabledAt": null }
            })
        );
    }
}
