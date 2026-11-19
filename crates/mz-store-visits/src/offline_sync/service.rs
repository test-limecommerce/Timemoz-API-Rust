//! Port `src/store-visits/mobile-offline-sync.service.ts` (+ `entities/offline-sync.entity.ts`,
//! `dto/offline-sync-query.dto.ts`): satu bundle offline — config company, wilayah, kunjungan aktif,
//! toko (scope ∪ jadwal ∪ kunjungan aktif, maks 2000), jadwal, schema form ter-publish.
//! Tidak membaca tabel produk sama sekali (katalog punya endpoint sendiri di `mz-products`).

use std::collections::HashMap;

use axum::http::StatusCode;
use chrono::NaiveDate;
use mz_core::company::require_company_id;
use mz_core::enums::PermissionAction;
use mz_core::jsfmt::num;
use mz_core::permissions::EffectiveAccess;
use mz_core::plan::{PlanFeature, company_gates};
use mz_core::time::JsDate;
use mz_core::{ApiError, ApiResult, AppState};
use mz_sales_geo::dto::parse_int_value;
use mz_table_fields::apply::apply_field_policy;
use mz_table_fields::policy::CensoredColumns;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, QueryBuilder};

use crate::plan_dates::{
    add_plan_days, from_plan_date, matches_plan_date_pattern, parse_plan_date, today_plan_date,
};
use crate::scope::{StoreScope, push_scope_clause, resolve_store_scope};

/// §0.6 — batas toko satu bundle.
pub const MAX_BUNDLE_STORES: i64 = 2000;
/// `MAX_SYNC_DAYS`.
pub const MAX_SYNC_DAYS: i64 = 7;
const SYNC_TOKEN_LENGTH: usize = 32;
const FORMS_FEATURE_KEY: &str = "store-visit-forms";

// ───────────────────────── DTO ─────────────────────────

/// `OfflineSyncQueryDto`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OfflineSyncQuery {
    pub date: Option<String>,
    pub days: Option<i64>,
}

/// Validasi query (`date`: `@IsOptional @Matches`; `days`: `toInt` → `@IsInt @Min(1) @Max(7)`).
pub fn parse_offline_sync_query(m: &Map<String, Value>) -> Result<OfflineSyncQuery, Vec<String>> {
    let mut errs = Vec::new();
    let mut q = OfflineSyncQuery::default();
    if let Some(v) = m.get("date").filter(|v| !v.is_null()) {
        match v {
            Value::String(s) if matches_plan_date_pattern(s) => q.date = Some(s.clone()),
            _ => errs.push("date must be a YYYY-MM-DD date".into()),
        }
    }
    let blank = |v: &Value| v.is_null() || v.as_str() == Some("");
    if let Some(v) = m.get("days").filter(|v| !blank(v)) {
        #[allow(clippy::cast_precision_loss)]
        let n = parse_int_value(v).map(|n| n as f64);
        if let Some(d) = mz_core::extract::check_int_range(&mut errs, "days", n, 1, MAX_SYNC_DAYS) {
            q.days = Some(d);
        }
    }
    if errs.is_empty() { Ok(q) } else { Err(errs) }
}

// ───────────────────────── pembantu murni ─────────────────────────

/// `canReadForms(access)`: tanpa `access` atau role bawaan → boleh; role custom butuh
/// `store-visit-forms:READ` (§0.12).
pub fn can_read_forms(access: Option<&EffectiveAccess>) -> bool {
    match access {
        None => true,
        Some(a) if a.is_system => true,
        Some(a) => a
            .permissions
            .get(FORMS_FEATURE_KEY)
            .is_some_and(|s| s.contains(&PermissionAction::READ)),
    }
}

/// `syncToken(content)`: sha256(JSON.stringify(content)) hex, 32 karakter pertama.
pub fn sync_token(content: &Value) -> String {
    let digest = Sha256::digest(content.to_string().as_bytes());
    let mut hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex.truncate(SYNC_TOKEN_LENGTH);
    hex
}

/// `planStatus` + `planVisitId`: kunjungan hidup (bukan `cancelled`) memenuhi jadwal.
pub fn plan_status(visit: Option<(&str, &str)>) -> (&'static str, Value) {
    match visit {
        Some((uuid, state)) if state != "cancelled" => ("fulfilled", Value::from(uuid)),
        _ => ("pending", Value::Null),
    }
}

/// `comparePlans`: tanggal, lalu `sortOrder` (null = ∞), lalu id.
pub fn compare_plans(a: &Value, b: &Value) -> std::cmp::Ordering {
    let s = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let o = |v: &Value| {
        v.get("sortOrder")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MAX)
    };
    s(a, "date")
        .cmp(&s(b, "date"))
        .then_with(|| o(a).cmp(&o(b)))
        .then_with(|| s(a, "id").cmp(&s(b, "id")))
}

/// `If-None-Match` cocok dengan `"<token>"` (daftar ber-koma, `W/` dibuang; `*` tidak cocok).
pub fn matches_etag(header: Option<&str>, token: &str) -> bool {
    let Some(h) = header.filter(|h| !h.is_empty()) else {
        return false;
    };
    let expected = format!("\"{token}\"");
    h.split(',')
        .map(str::trim)
        .map(|t| t.strip_prefix("W/").unwrap_or(t))
        .any(|t| t == expected)
}

// ───────────────────────── baris ─────────────────────────

#[derive(Debug, Clone, sqlx::FromRow)]
struct CompanyConfig {
    #[sqlx(rename = "timeZone")]
    time_zone: String,
    #[sqlx(rename = "storeVisitDefaultRadiusMeters")]
    default_radius: i32,
    #[sqlx(rename = "storeVisitMaxAccuracyMeters")]
    max_accuracy: i32,
    #[sqlx(rename = "storeVisitBlockMockLocation")]
    block_mock: bool,
    #[sqlx(rename = "storeVisitRequirePhotos")]
    require_photos: bool,
    #[sqlx(rename = "storeVisitAllowOverride")]
    allow_override: bool,
    #[sqlx(rename = "storeVisitMinDurationMinutes")]
    min_duration: i32,
    #[sqlx(rename = "storeVisitMaxDurationHours")]
    max_duration: i32,
    #[sqlx(rename = "storeVisitEnforceGroupScope")]
    enforce: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct PlanRow {
    uuid: String,
    #[sqlx(rename = "visitDate")]
    visit_date: NaiveDate,
    #[sqlx(rename = "sortOrder")]
    sort_order: Option<i32>,
    note: Option<String>,
    #[sqlx(rename = "storeId")]
    store_id: i32,
    store_uuid: String,
    visit_uuid: Option<String>,
    visit_state: Option<String>,
    vt_uuid: Option<String>,
    vt_code: Option<String>,
    vt_name: Option<String>,
    vt_requires_order: Option<bool>,
    vt_requires_photo: Option<bool>,
    vt_requires_gps: Option<bool>,
    vt_is_active: Option<bool>,
}

impl PlanRow {
    fn to_json(&self) -> Value {
        let (status, visit_id) =
            plan_status(self.visit_uuid.as_deref().zip(self.visit_state.as_deref()));
        let visit_type = match (&self.vt_uuid, &self.vt_code, &self.vt_name) {
            (Some(id), Some(code), Some(name)) => json!({
                "id": id,
                "code": code,
                "name": name,
                "requiresOrder": self.vt_requires_order.unwrap_or(false),
                "requiresPhoto": self.vt_requires_photo.unwrap_or(false),
                "requiresGps": self.vt_requires_gps.unwrap_or(true),
                "isActive": self.vt_is_active.unwrap_or(true),
            }),
            _ => Value::Null,
        };
        json!({
            "id": self.uuid,
            "date": from_plan_date(self.visit_date),
            "storeId": self.store_uuid,
            "sortOrder": self.sort_order,
            "note": self.note,
            "status": status,
            "visitId": visit_id,
            "visitType": visit_type,
        })
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct StoreRow {
    uuid: String,
    code: String,
    name: String,
    #[sqlx(rename = "ownerName")]
    owner_name: String,
    phone: Option<String>,
    address: String,
    lat: f64,
    lng: f64,
    #[sqlx(rename = "radiusMeters")]
    radius_meters: Option<i32>,
    #[sqlx(rename = "lastVisitAt")]
    last_visit_at: Option<JsDate>,
    group_uuid: Option<String>,
    group_name: Option<String>,
    customer_uuid: Option<String>,
    customer_name: Option<String>,
}

impl StoreRow {
    /// `toStore(row)`.
    fn to_json(&self) -> Value {
        let pair = |id: &Option<String>, name: &Option<String>| match (id, name) {
            (Some(i), Some(n)) => json!({ "id": i, "name": n }),
            _ => Value::Null,
        };
        json!({
            "id": self.uuid,
            "code": self.code,
            "name": self.name,
            "ownerName": self.owner_name,
            "phone": self.phone,
            "address": self.address,
            "lat": num(self.lat),
            "lng": num(self.lng),
            "radiusMeters": self.radius_meters,
            "group": pair(&self.group_uuid, &self.group_name),
            "customer": pair(&self.customer_uuid, &self.customer_name),
            "lastVisitAt": self.last_visit_at.map(|d| d.timestamp_millis()),
        })
    }
}

fn invalid_date() -> ApiError {
    ApiError::coded(
        StatusCode::BAD_REQUEST,
        "INVALID_DATE_RANGE",
        "date is not a valid calendar date",
    )
}

/// `storeWhere(companyId, scope, extraStoreIds)` → ` WHERE ...` atas `store s`.
fn push_store_where(
    qb: &mut QueryBuilder<'_, Postgres>,
    company_id: i32,
    scope: &StoreScope,
    extra: &[i32],
) {
    qb.push(r#" WHERE s."companyId" = "#)
        .push_bind(company_id)
        .push(r#" AND s."deletedAt" IS NULL AND s."isActive" = true"#);
    if scope.is_none() {
        return;
    }
    // `OR: [scopeClause, { id: { in: extra } }]` — `true AND (scope)` mengikat lebih kuat dari OR.
    qb.push(" AND (true");
    push_scope_clause(qb, scope, r#"s."groupId""#);
    if !extra.is_empty() {
        qb.push(" OR s.id = ANY(")
            .push_bind(extra.to_vec())
            .push(")");
    }
    qb.push(")");
}

const STORE_SELECT: &str = r#"SELECT s.uuid::text AS uuid, s.code, s.name, s."ownerName", s.phone, s.address,
       s.lat, s.lng, s."radiusMeters", s."lastVisitAt",
       g.uuid::text AS group_uuid, g.name AS group_name,
       c.uuid::text AS customer_uuid, c.name AS customer_name
  FROM store s
  LEFT JOIN "storeGroup" g ON g.id = s."groupId"
  LEFT JOIN customer c ON c.id = s."customerId""#;

/// `resolveFormTemplates(companyId, userId)`: versi ter-publish TERTINGGI per template yang
/// berlaku untuk sales ini (scope `all` atau assignment hidup).
async fn resolve_form_templates(
    db: &sqlx::PgPool,
    company_id: i32,
    user_id: i32,
) -> ApiResult<Vec<Value>> {
    let templates: Vec<(i32, String, String, Option<String>, i32)> = sqlx::query_as(
        r#"SELECT t.id, t.uuid::text, t.name, t.description, t."sortOrder"
             FROM "storeVisitFormTemplate" t
            WHERE t."companyId" = $1 AND t."deletedAt" IS NULL AND t."isActive" = true
              AND EXISTS (SELECT 1 FROM "storeVisitFormTemplateVersion" v
                           WHERE v."templateId" = t.id AND v."publishedAt" IS NOT NULL)
              AND (t.scope = 'all'::"StoreVisitFormScope"
                   OR EXISTS (SELECT 1 FROM "storeVisitFormTemplateUser" a
                               WHERE a."templateId" = t.id AND a."userId" = $2 AND a."deletedAt" IS NULL))
            ORDER BY t."sortOrder" ASC, t.name ASC, t.id ASC"#,
    )
    .bind(company_id)
    .bind(user_id)
    .fetch_all(db)
    .await?;
    if templates.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i32> = templates.iter().map(|t| t.0).collect();
    let versions: Vec<(String, i32, i32, Value)> = sqlx::query_as(
        r#"SELECT uuid::text, version, "templateId", schema FROM "storeVisitFormTemplateVersion"
            WHERE "templateId" = ANY($1) AND "publishedAt" IS NOT NULL"#,
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;
    let mut latest: HashMap<i32, (String, i32, Value)> = HashMap::new();
    for (uuid, version, tid, schema) in versions {
        if latest.get(&tid).is_none_or(|cur| version > cur.1) {
            latest.insert(tid, (uuid, version, schema));
        }
    }
    Ok(templates
        .into_iter()
        .filter_map(|(id, uuid, name, description, sort_order)| {
            let (vid, version, schema) = latest.remove(&id)?;
            Some(json!({
                "id": uuid,
                "name": name,
                "description": description,
                "sortOrder": sort_order,
                "versionId": vid,
                "version": version,
                "schema": schema,
            }))
        })
        .collect())
}

/// `sync(userId, query, censored, access)` → bundle (`syncedAt`, `syncToken`, isi).
pub async fn sync(
    state: &AppState,
    user_id: i32,
    q: &OfflineSyncQuery,
    censored: &CensoredColumns,
    access: Option<&EffectiveAccess>,
) -> ApiResult<Value> {
    let db = &state.db;
    let company_id = require_company_id(db, user_id).await?;
    let start_date = match &q.date {
        Some(d) => {
            parse_plan_date(d).ok_or_else(invalid_date)?;
            d.clone()
        }
        None => today_plan_date(),
    };
    let days = q.days.unwrap_or(1);
    let end_date = add_plan_days(&start_date, days - 1).ok_or_else(invalid_date)?;
    let (start, end) = (
        parse_plan_date(&start_date).ok_or_else(invalid_date)?,
        parse_plan_date(&end_date).ok_or_else(invalid_date)?,
    );

    let company: Option<CompanyConfig> = sqlx::query_as(
        r#"SELECT "timeZone", "storeVisitDefaultRadiusMeters", "storeVisitMaxAccuracyMeters",
                  "storeVisitBlockMockLocation", "storeVisitRequirePhotos", "storeVisitAllowOverride",
                  "storeVisitMinDurationMinutes", "storeVisitMaxDurationHours", "storeVisitEnforceGroupScope"
             FROM company WHERE id = $1"#,
    )
    .bind(company_id)
    .fetch_optional(db)
    .await?;
    let enforce = company.as_ref().is_some_and(|c| c.enforce);

    let plans_fut = async {
        Ok::<Vec<PlanRow>, ApiError>(
            sqlx::query_as(
                r#"SELECT p.uuid::text AS uuid, p."visitDate", p."sortOrder", p.note, p."storeId",
                          s.uuid::text AS store_uuid, sv.uuid::text AS visit_uuid, sv.state::text AS visit_state,
                          vt.uuid::text AS vt_uuid, vt.code AS vt_code, vt.name AS vt_name,
                          vt."requiresOrder" AS vt_requires_order, vt."requiresPhoto" AS vt_requires_photo,
                          vt."requiresGps" AS vt_requires_gps, vt."isActive" AS vt_is_active
                     FROM "storeVisitPlan" p
                     JOIN store s ON s.id = p."storeId"
                     LEFT JOIN "storeVisit" sv ON sv."planId" = p.id
                     LEFT JOIN "visitType" vt ON vt.id = p."visitTypeId"
                    WHERE p."companyId" = $1 AND p."userId" = $2 AND p."deletedAt" IS NULL
                      AND p."visitDate" >= $3 AND p."visitDate" <= $4
                      AND s."deletedAt" IS NULL AND s."isActive" = true
                    ORDER BY p.id"#,
            )
            .bind(company_id)
            .bind(user_id)
            .bind(start)
            .bind(end)
            .fetch_all(db)
            .await?,
        )
    };
    let active_fut = async {
        Ok::<Option<(String, String, JsDate, i32, String)>, ApiError>(
            sqlx::query_as(
                r#"SELECT v.uuid::text, v."visitNumber", v."checkInAt", v."storeId", s.uuid::text
                     FROM "storeVisit" v JOIN store s ON s.id = v."storeId"
                    WHERE v."companyId" = $1 AND v."userId" = $2 AND v.state = 'active'
                    ORDER BY v.id LIMIT 1"#,
            )
            .bind(company_id)
            .bind(user_id)
            .fetch_optional(db)
            .await?,
        )
    };
    let (scope, plan_rows, active) = tokio::try_join!(
        resolve_store_scope(db, user_id, company_id, Some(enforce)),
        plans_fut,
        active_fut
    )?;

    let mut planned_store_ids: Vec<i32> = Vec::new();
    for id in plan_rows
        .iter()
        .map(|r| r.store_id)
        .chain(active.as_ref().map(|a| a.3))
    {
        if !planned_store_ids.contains(&id) {
            planned_store_ids.push(id);
        }
    }

    let mut count_qb = QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM store s");
    push_store_where(&mut count_qb, company_id, &scope, &planned_store_ids);
    let mut rows_qb = QueryBuilder::<Postgres>::new(STORE_SELECT);
    push_store_where(&mut rows_qb, company_id, &scope, &planned_store_ids);
    rows_qb
        .push(" ORDER BY s.name ASC, s.id ASC LIMIT ")
        .push_bind(MAX_BUNDLE_STORES + 1);
    let (store_count, store_rows): (i64, Vec<StoreRow>) = tokio::try_join!(
        async { Ok::<i64, ApiError>(count_qb.build_query_scalar().fetch_one(db).await?) },
        async { Ok::<Vec<StoreRow>, ApiError>(rows_qb.build_query_as().fetch_all(db).await?) },
    )?;
    if store_count > MAX_BUNDLE_STORES {
        return Err(ApiError::body(
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({
                "code": "OFFLINE_BUNDLE_TOO_LARGE",
                "message": "Too many stores in scope for an offline bundle",
                "details": { "storeCount": store_count, "limit": MAX_BUNDLE_STORES },
            }),
        ));
    }

    let include_forms = can_read_forms(access)
        && company_gates(db, company_id)
            .await?
            .is_none_or(|g| g.has(PlanFeature::Form));
    let form_templates = if include_forms {
        resolve_form_templates(db, company_id, user_id).await?
    } else {
        Vec::new()
    };

    let scope_groups: Vec<(String, String)> =
        match scope.as_ref().filter(|s| !s.group_ids.is_empty()) {
            None => Vec::new(),
            Some(s) => sqlx::query_as(
                r#"SELECT uuid::text, name FROM "storeGroup" WHERE id = ANY($1) AND "companyId" = $2
                    ORDER BY name ASC"#,
            )
            .bind(&s.group_ids)
            .bind(company_id)
            .fetch_all(db)
            .await?,
        };

    let mut stores = Value::Array(store_rows.iter().map(StoreRow::to_json).collect());
    apply_field_policy(&mut stores, "stores", censored, None);
    let stores_len = stores.as_array().map_or(0, Vec::len);

    let mut plans: Vec<Value> = plan_rows.iter().map(PlanRow::to_json).collect();
    plans.sort_by(compare_plans);

    let active_visit = active.map_or(Value::Null, |(uuid, number, at, _, store_uuid)| {
        json!({
            "id": uuid,
            "storeId": store_uuid,
            "visitNumber": number,
            "checkInAt": at.timestamp_millis(),
        })
    });

    let config = match &company {
        Some(c) => json!({
            "timeZone": c.time_zone,
            "storeVisitDefaultRadiusMeters": c.default_radius,
            "storeVisitMaxAccuracyMeters": c.max_accuracy,
            "storeVisitBlockMockLocation": c.block_mock,
            "storeVisitRequirePhotos": c.require_photos,
            "storeVisitAllowOverride": c.allow_override,
            "storeVisitMinDurationMinutes": c.min_duration,
            "storeVisitMaxDurationHours": c.max_duration,
            "storeVisitEnforceGroupScope": enforce,
        }),
        None => json!({
            "timeZone": null,
            "storeVisitDefaultRadiusMeters": 0,
            "storeVisitMaxAccuracyMeters": 0,
            "storeVisitBlockMockLocation": false,
            "storeVisitRequirePhotos": false,
            "storeVisitAllowOverride": false,
            "storeVisitMinDurationMinutes": 0,
            "storeVisitMaxDurationHours": 0,
            "storeVisitEnforceGroupScope": enforce,
        }),
    };

    let plans_len = plans.len();
    let forms_len = form_templates.len();
    let mut content = Map::new();
    content.insert(
        "range".into(),
        json!({ "startDate": start_date, "endDate": end_date, "days": days }),
    );
    content.insert("config".into(), config);
    content.insert(
        "scope".into(),
        json!({
            "unrestricted": scope.is_none(),
            "strict": enforce,
            "groups": scope_groups.iter().map(|(id, name)| json!({ "id": id, "name": name })).collect::<Vec<_>>(),
        }),
    );
    content.insert("activeVisit".into(), active_visit);
    content.insert("stores".into(), stores);
    content.insert("plans".into(), Value::Array(plans));
    if include_forms {
        content.insert("formTemplates".into(), Value::Array(form_templates));
    }
    content.insert(
        "counts".into(),
        json!({ "stores": stores_len, "plans": plans_len, "formTemplates": forms_len }),
    );
    let content = Value::Object(content);
    let token = sync_token(&content);

    let mut out = Map::new();
    out.insert(
        "syncedAt".into(),
        Value::from(chrono::Utc::now().timestamp_millis()),
    );
    out.insert("syncToken".into(), Value::from(token));
    if let Value::Object(c) = content {
        out.extend(c);
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn q(v: Value) -> Result<OfflineSyncQuery, Vec<String>> {
        parse_offline_sync_query(v.as_object().unwrap())
    }

    #[test]
    fn query_dto() {
        assert_eq!(q(json!({})).unwrap(), OfflineSyncQuery::default());
        assert_eq!(
            q(json!({"date": "2026-08-20", "days": "3"})).unwrap(),
            OfflineSyncQuery {
                date: Some("2026-08-20".into()),
                days: Some(3)
            }
        );
        assert_eq!(q(json!({"days": ""})).unwrap().days, None);
        assert_eq!(
            q(json!({"date": "20-08-2026"})).unwrap_err(),
            vec!["date must be a YYYY-MM-DD date"]
        );
        assert_eq!(
            q(json!({"days": "8"})).unwrap_err(),
            vec!["days must not be greater than 7"]
        );
        assert_eq!(
            q(json!({"days": "x"})).unwrap_err(),
            vec![
                "days must not be greater than 7",
                "days must not be less than 1",
                "days must be an integer number"
            ]
        );
    }

    fn access(is_system: bool, perms: &[(&str, PermissionAction)]) -> EffectiveAccess {
        let mut permissions: HashMap<String, HashSet<PermissionAction>> = HashMap::new();
        for (k, a) in perms {
            permissions.entry((*k).to_owned()).or_default().insert(*a);
        }
        EffectiveAccess {
            role_id: 1,
            role_code: "STAFF".into(),
            role_name: "Staff".into(),
            is_system,
            base_role: None,
            effective_role_code: "STAFF".into(),
            company_id: 2,
            permissions,
        }
    }

    #[test]
    fn forms_gate_by_role() {
        assert!(can_read_forms(None));
        assert!(can_read_forms(Some(&access(true, &[]))));
        assert!(!can_read_forms(Some(&access(false, &[]))));
        assert!(!can_read_forms(Some(&access(
            false,
            &[("store-visit-forms", PermissionAction::WRITE)]
        ))));
        assert!(can_read_forms(Some(&access(
            false,
            &[("store-visit-forms", PermissionAction::READ)]
        ))));
    }

    #[test]
    fn token_is_stable_32_hex_and_content_sensitive() {
        let a = json!({"stores": [{"customer": {"name": "A"}}]});
        let b = json!({"stores": [{"customer": {"name": "B"}}]});
        let t = sync_token(&a);
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(t, sync_token(&a.clone()));
        assert_ne!(t, sync_token(&b));
    }

    #[test]
    fn plan_status_follows_live_visit() {
        assert_eq!(plan_status(None), ("pending", Value::Null));
        assert_eq!(
            plan_status(Some(("v", "cancelled"))),
            ("pending", Value::Null)
        );
        assert_eq!(
            plan_status(Some(("v", "active"))),
            ("fulfilled", json!("v"))
        );
        assert_eq!(
            plan_status(Some(("v", "completed"))),
            ("fulfilled", json!("v"))
        );
    }

    #[test]
    fn plans_ordering() {
        let p = |id: &str, date: &str, so: Option<i32>| json!({"id": id, "date": date, "sortOrder": so});
        let mut v = [
            p("c", "2026-08-21", Some(1)),
            p("b", "2026-08-20", None),
            p("a", "2026-08-20", None),
            p("z", "2026-08-20", Some(2)),
        ];
        v.sort_by(compare_plans);
        let ids: Vec<&str> = v.iter().map(|x| x["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["z", "a", "b", "c"]);
    }

    #[test]
    fn etag_matching() {
        assert!(!matches_etag(None, "abc"));
        assert!(!matches_etag(Some(""), "abc"));
        assert!(matches_etag(Some("\"abc\""), "abc"));
        assert!(matches_etag(Some("W/\"x\", W/\"abc\""), "abc"));
        assert!(!matches_etag(Some("*"), "abc"));
        assert!(!matches_etag(Some("abc"), "abc"));
    }

    #[test]
    fn store_shape_and_censoring() {
        let row = StoreRow {
            uuid: "s".into(),
            code: "C".into(),
            name: "N".into(),
            owner_name: "O".into(),
            phone: Some("+62".into()),
            address: "A".into(),
            lat: -7.0,
            lng: 110.5,
            radius_meters: Some(50),
            last_visit_at: JsDate::from_millis(9),
            group_uuid: None,
            group_name: None,
            customer_uuid: Some("c".into()),
            customer_name: Some("Cust".into()),
        };
        let v = row.to_json();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "id",
                "code",
                "name",
                "ownerName",
                "phone",
                "address",
                "lat",
                "lng",
                "radiusMeters",
                "group",
                "customer",
                "lastVisitAt"
            ]
        );
        assert_eq!(v["lat"], json!(-7));
        assert_eq!(v["customer"], json!({"id": "c", "name": "Cust"}));
        assert!(v.get("qrToken").is_none());
        let mut arr = Value::Array(vec![v.clone()]);
        let censored: CensoredColumns = [(
            "radiusMeters".to_owned(),
            mz_table_fields::registry::FieldVisibilityMode::Hidden,
        )]
        .into();
        let before = sync_token(&arr);
        apply_field_policy(&mut arr, "stores", &censored, None);
        assert_eq!(arr[0]["radiusMeters"], Value::Null);
        assert_ne!(before, sync_token(&arr));
    }

    #[test]
    fn store_where_union() {
        let scope = Some(crate::scope::Scope {
            group_ids: vec![1],
            strict: true,
        });
        let mut qb = QueryBuilder::<Postgres>::new("SELECT 1 FROM store s");
        push_store_where(&mut qb, 2, &scope, &[5]);
        assert!(
            qb.sql()
                .ends_with(r#" AND (true AND (s."groupId" = ANY($2)) OR s.id = ANY($3))"#),
            "{}",
            qb.sql()
        );
        let mut qb = QueryBuilder::<Postgres>::new("SELECT 1 FROM store s");
        push_store_where(&mut qb, 2, &None, &[5]);
        assert!(qb.sql().ends_with(r#""isActive" = true"#));
    }
}
