//! `src/admin-company/features/*` — saklar fitur company + reset kuota export (3 route).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::NaiveDateTime;
use mz_audit::{AuditHandle, Lookup, Model, Row};
use mz_core::plan::PlanFeature;
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState};
use mz_operator::{AuditCtx, Operator, OperatorAuditEntry, record};
use serde_json::{Map, Value, json};

use crate::common::{
    ALL_ROLES, CompanyChange, OWNER_SUPPORT, RawBody, after_company_change, body_value, coded,
    company_not_found, date, date_of, gate, ok, uuid_pipe,
};
use crate::plan_limits::current_sub;
use crate::val::{V, absent, as_obj, trim_field};

type Op = Result<Operator, ApiError>;

/// `FEATURE_SWITCH_KEYS` (urutan dipakai audit & cek peran).
pub const FEATURE_SWITCH_KEYS: [&str; 6] = [
    "moduleTimeTrackingEnabled",
    "moduleSalesTrackerEnabled",
    "customRolesEnabled",
    "divisionsEnabled",
    "dontChargeInvite",
    "isAgencyMode",
];

const CUSTOM_ROLES_REASON: &str = "Permission enforcement is always on for every role.";

#[derive(Debug, Clone, sqlx::FromRow)]
struct FeatureRow {
    id: i32,
    uuid: uuid::Uuid,
    #[sqlx(rename = "moduleTimeTrackingEnabled")]
    time: bool,
    #[sqlx(rename = "moduleSalesTrackerEnabled")]
    sales: bool,
    #[sqlx(rename = "customRolesEnabled")]
    custom_roles: bool,
    #[sqlx(rename = "divisionsEnabled")]
    divisions: bool,
    #[sqlx(rename = "dontChargeInvite")]
    dont_charge_invite: bool,
    #[sqlx(rename = "isAgencyMode")]
    agency: bool,
    #[sqlx(rename = "reportExportsPerMonth")]
    per_month: i32,
    #[sqlx(rename = "reportExportsUsedThisMonth")]
    used: i32,
    #[sqlx(rename = "reportExportsResetAt")]
    reset_at: Option<NaiveDateTime>,
}

impl FeatureRow {
    fn get(&self, key: &str) -> Option<bool> {
        Some(match key {
            "moduleTimeTrackingEnabled" => self.time,
            "moduleSalesTrackerEnabled" => self.sales,
            "customRolesEnabled" => self.custom_roles,
            "divisionsEnabled" => self.divisions,
            "dontChargeInvite" => self.dont_charge_invite,
            "isAgencyMode" => self.agency,
            _ => return None,
        })
    }

    fn set(&mut self, key: &str, v: bool) {
        match key {
            "moduleTimeTrackingEnabled" => self.time = v,
            "moduleSalesTrackerEnabled" => self.sales = v,
            "customRolesEnabled" => self.custom_roles = v,
            "divisionsEnabled" => self.divisions = v,
            "dontChargeInvite" => self.dont_charge_invite = v,
            "isAgencyMode" => self.agency = v,
            _ => {}
        }
    }
}

const SELECT: &str = r#"SELECT id, uuid, "moduleTimeTrackingEnabled", "moduleSalesTrackerEnabled",
       "customRolesEnabled", "divisionsEnabled", "dontChargeInvite", "isAgencyMode",
       "reportExportsPerMonth", "reportExportsUsedThisMonth", "reportExportsResetAt"
  FROM company WHERE uuid = $1 AND status <> 0"#;

/// Bentuk GET: saklar + gerbang efektif (dihitung segar).
async fn view(state: &AppState, c: &FeatureRow) -> ApiResult<Map<String, Value>> {
    let plan_feature = match mz_core::plan::company_gates(&state.db, c.id).await? {
        None => true,
        Some(g) => g.has(PlanFeature::Divisions),
    };
    let boost: bool = sqlx::query_scalar(
        r#"SELECT EXISTS (SELECT 1 FROM "companyPlanBoost" WHERE "companyId" = $1 AND enabled = true
             AND product::text IN ('SALES', 'TIME'))"#,
    )
    .bind(c.id)
    .fetch_one(&state.db)
    .await?;
    let product = current_sub(&state.db, c.id)
        .await?
        .and_then(|s| s.tier().and_then(|t| t.get("productType")).cloned())
        .unwrap_or(Value::Null);
    let active = c.divisions && c.time && plan_feature;
    let flag = |on: bool| if on { "ON" } else { "OFF" };
    let explanation = format!(
        "Divisions: switch {} · Time module {} · plan {} divisions → {}",
        flag(c.divisions),
        flag(c.time),
        if plan_feature {
            "includes"
        } else {
            "does NOT include"
        },
        if active { "active" } else { "not active" }
    );
    let v = json!({
        "moduleTimeTrackingEnabled": c.time,
        "moduleSalesTrackerEnabled": c.sales,
        "customRolesEnabled": { "value": c.custom_roles, "effective": false, "reason": CUSTOM_ROLES_REASON },
        "divisions": {
            "switch": c.divisions,
            "timeModule": c.time,
            "planFeature": plan_feature,
            "boost": boost,
            "active": active,
            "explanation": explanation,
        },
        "dontChargeInvite": c.dont_charge_invite,
        "isAgencyMode": c.agency,
        "reportExports": { "perMonth": c.per_month, "usedThisMonth": c.used, "resetAt": date(c.reset_at) },
        "planProductType": product,
    });
    Ok(match v {
        Value::Object(m) => m,
        _ => Map::new(),
    })
}

/// Node: src/admin-company/features/company-features.controller.ts:42 (`GET /admin-company/:uuid/features`)
pub async fn h_get(
    State(state): State<AppState>,
    op: Op,
    audit: AuditHandle,
    Path(raw): Path<String>,
) -> ApiResult<Json<Value>> {
    gate(op, &audit, None, Some(ALL_ROLES))?;
    let uuid = uuid_pipe(&raw)?;
    let row: Option<FeatureRow> = sqlx::query_as(&format!("{SELECT} ORDER BY id LIMIT 1"))
        .bind(uuid)
        .fetch_optional(&state.db)
        .await?;
    let Some(c) = row else {
        return Err(company_not_found());
    };
    Ok(Json(ok(Value::Object(view(&state, &c).await?))))
}

/// `PatchFeaturesDto` (whitelist). Hasil: body ter-trim.
pub fn validate_patch(body: &Value) -> ApiResult<Map<String, Value>> {
    let mut m = as_obj(body);
    trim_field(&mut m, "reason");
    let mut v = V::new();
    for e in crate::val::array_keys(body) {
        v.push(e);
    }
    let mut allowed: Vec<&str> = FEATURE_SWITCH_KEYS.to_vec();
    allowed.push("reason");
    v.whitelist(&m, &allowed);
    for k in FEATURE_SWITCH_KEYS {
        if !absent(m.get(k)) {
            v.is_boolean(k, m.get(k));
        }
    }
    // `@Validate(AtLeastOneFeatureSwitch)` lalu Length, IsString.
    if !FEATURE_SWITCH_KEYS.iter().any(|k| m.contains_key(*k)) {
        v.push(format!(
            "at least one of {} is required",
            FEATURE_SWITCH_KEYS.join(", ")
        ));
    }
    v.reason(m.get("reason"));
    v.finish()?;
    Ok(m)
}

fn forbidden_field(role: &str, field: &str) -> ApiError {
    ApiError::body(
        StatusCode::FORBIDDEN,
        json!({
            "code": "OPERATOR_ROLE_FORBIDDEN",
            "message": format!("Operator role {role} may not change {field}"),
            "field": field,
        }),
    )
}

fn at_least_one_module() -> ApiError {
    coded(
        StatusCode::BAD_REQUEST,
        "AT_LEAST_ONE_MODULE",
        "At least one module must stay enabled",
    )
}

/// Node: src/admin-company/features/company-features.controller.ts:53 (`PATCH /admin-company/:uuid/features`)
pub async fn h_patch(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw): Path<String>,
    body: RawBody,
) -> ApiResult<Json<Value>> {
    let op = gate(op, &audit, Some(&body), Some(ALL_ROLES))?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let dto = validate_patch(&body_value(body))?;
    let uuid = uuid_pipe(&raw)?;
    let reason = dto
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    // Hanya kunci saklar bernilai boolean, urutan kemunculan di body.
    let requested: Vec<(String, bool)> = dto
        .iter()
        .filter(|(k, _)| FEATURE_SWITCH_KEYS.contains(&k.as_str()))
        .filter_map(|(k, v)| v.as_bool().map(|b| (k.clone(), b)))
        .collect();
    for (field, _) in &requested {
        let allowed = if field == "dontChargeInvite" {
            op.role == "OWNER" || op.role == "FINANCE"
        } else {
            op.role == "OWNER" || op.role == "SUPPORT"
        };
        if !allowed {
            return Err(forbidden_field(&op.role, field));
        }
    }

    let mut tx = state.db.begin().await?;
    let row: Option<FeatureRow> = sqlx::query_as(&format!("{SELECT} FOR UPDATE"))
        .bind(uuid)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(current) = row else {
        return Err(company_not_found());
    };
    let changed: Vec<(String, bool)> = requested
        .into_iter()
        .filter(|(k, next)| current.get(k) != Some(*next))
        .collect();
    let mut updated = current.clone();
    let did_change = !changed.is_empty();
    if did_change {
        for (k, v) in &changed {
            updated.set(k, *v);
        }
        if !updated.time && !updated.sales {
            return Err(at_least_one_module());
        }
        if changed.iter().any(|(k, v)| k == "customRolesEnabled" && *v) {
            let global: i64 = sqlx::query_scalar(
                r#"SELECT COUNT(*) FROM "rolePermission" rp JOIN "accessLevel" al ON al.id = rp."accessLevelId"
                   WHERE al."companyId" IS NULL"#,
            )
            .fetch_one(&mut *tx)
            .await?;
            if global == 0 {
                return Err(coded(
                    StatusCode::CONFLICT,
                    "ROLE_PERMISSIONS_NOT_BACKFILLED",
                    "Global built-in role permission matrix is empty; run the roles backfill first",
                ));
            }
        }

        let after: Map<String, Value> = changed
            .iter()
            .map(|(k, v)| (k.clone(), Value::Bool(*v)))
            .collect();
        let before: Map<String, Value> = changed
            .iter()
            .map(|(k, _)| (k.clone(), current.get(k).map_or(Value::Null, Value::Bool)))
            .collect();

        // `tx.company.update({ where: { id }, data: after })` + audit.
        let pre = audit.pre_read(Model::Company, Lookup::Id(current.id)).await;
        let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new("UPDATE company SET ");
        for (n, (k, v)) in changed.iter().enumerate() {
            if n > 0 {
                qb.push(", ");
            }
            qb.push(format!("\"{k}\" = ")).push_bind(*v);
        }
        qb.push(" WHERE id = ").push_bind(current.id);
        if let Err(e) = qb.build().execute(&mut *tx).await {
            // `company_at_least_one_module` (check_violation 23514).
            if let sqlx::Error::Database(db) = &e
                && (db.code().as_deref() == Some("23514")
                    || db.message().contains("company_at_least_one_module"))
            {
                return Err(at_least_one_module());
            }
            return Err(e.into());
        }
        if let Some(r) = audit
            .result_row(&mut *tx, Model::Company, Lookup::Id(current.id))
            .await
        {
            audit.record_update(Model::Company, pre, &r).await;
        }
        let after_v = Value::Object(after);
        let before_v = Value::Object(before);
        record(
            &mut *tx,
            &ctx,
            &OperatorAuditEntry {
                operator_id: op.id,
                action: "company.features",
                target_type: "company",
                target_uuid: Some(current.uuid),
                company_id: Some(current.id),
                before: Some(&before_v),
                after: Some(&after_v),
                reason: Some(&reason),
            },
        )
        .await?;
    }
    tx.commit().await?;

    if did_change {
        after_company_change(
            &state,
            updated.id,
            CompanyChange {
                modules: true,
                divisions: true,
                plan: true,
                user_me: true,
            },
        )
        .await;
    }

    Ok(Json(ok(Value::Object(view(&state, &updated).await?))))
}

/// Node: src/admin-company/features/company-features.controller.ts:74 (`POST /admin-company/:uuid/report-exports/reset`)
pub async fn h_reset_exports(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw): Path<String>,
    body: RawBody,
) -> ApiResult<impl IntoResponse> {
    let op = gate(op, &audit, Some(&body), Some(OWNER_SUPPORT))?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let reason = crate::lifecycle::validate_reason_dto(&body_value(body))?;
    let uuid = uuid_pipe(&raw)?;

    let now = now_naive();
    let mut tx = state.db.begin().await?;
    let u = uuid;
    let pre = audit
        .pre_read_many(
            Model::Company,
            Lookup::filter(move |qb| {
                qb.push("uuid = ").push_bind(u).push(" AND status <> 0");
            }),
        )
        .await;
    let n = sqlx::query(
        r#"UPDATE company SET "reportExportsUsedThisMonth" = 0, "reportExportsResetAt" = $2
           WHERE uuid = $1 AND status <> 0"#,
    )
    .bind(uuid)
    .bind(now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    audit
        .record_update_many(
            Model::Company,
            pre,
            n,
            &Row::new()
                .with("reportExportsUsedThisMonth", 0)
                .with("reportExportsResetAt", date_of(now)),
            json!({ "uuid": uuid.to_string(), "status": { "not": 0 } }),
        )
        .await;
    if n == 0 {
        return Err(company_not_found());
    }
    let company_id: Option<i32> = sqlx::query_scalar("SELECT id FROM company WHERE uuid = $1")
        .bind(uuid)
        .fetch_optional(&mut *tx)
        .await?;
    let after = json!({ "reportExportsUsedThisMonth": 0, "reportExportsResetAt": date_of(now) });
    record(
        &mut *tx,
        &ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "company.report_exports_reset",
            target_type: "company",
            target_uuid: Some(uuid),
            company_id,
            before: None,
            after: Some(&after),
            reason: Some(&reason),
        },
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(ok(after))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errs(body: Value) -> Vec<String> {
        match validate_patch(&body) {
            Err(ApiError::Validation(m)) => m,
            _ => Vec::new(),
        }
    }

    #[test]
    fn patch_requires_one_switch_and_reason() {
        assert_eq!(
            errs(json!({ "foo": 1 })),
            vec![
                "property foo should not exist",
                "at least one of moduleTimeTrackingEnabled, moduleSalesTrackerEnabled, customRolesEnabled, divisionsEnabled, dontChargeInvite, isAgencyMode is required",
                "reason must be longer than or equal to 5 characters",
                "reason must be a string",
            ]
        );
        assert_eq!(
            errs(json!({ "isAgencyMode": "yes", "reason": "   valid reason  " })),
            vec!["isAgencyMode must be a boolean value"]
        );
        // `null` memenuhi "minimal satu" (≠ undefined) dan dilewati IsOptional.
        assert!(errs(json!({ "isAgencyMode": null, "reason": "valid reason" })).is_empty());
        assert_eq!(
            errs(json!({ "isAgencyMode": true, "reason": "  abc  " })),
            vec!["reason must be longer than or equal to 5 characters"]
        );
    }
}
