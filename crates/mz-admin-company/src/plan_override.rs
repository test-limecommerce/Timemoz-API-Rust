//! `src/admin-company/plan-override/*` — override batas paket per company (3 route).

use axum::Json;
use axum::extract::{Path, State};
use chrono::NaiveDateTime;
use mz_audit::AuditHandle;
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState};
use mz_operator::{AuditCtx, Operator, OperatorAuditEntry, record};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use sqlx::types::Json as SqlJson;

use crate::common::{
    ALL_ROLES, CompanyChange, OWNER_FINANCE, RawBody, after_company_change, body_value,
    company_not_found, date_of, gate, lock_company_by_uuid, ok, uuid_pipe,
};
use crate::plan_limits::{
    CurrentSub, OVERRIDE_JSON, SALES_LIMIT_FIELDS, TIME_LIMIT_FIELDS, UNLIMITED,
    apply_company_plan_limits, current_sub, obj, resolve_override_value, unlimited_of,
};
use crate::val::{V, absent, array_keys, as_obj, num_of, trim_field};

type Op = Result<Operator, ApiError>;

/// `OVERRIDE_LIMIT_FIELDS` = SALES lalu TIME.
pub fn override_limit_fields() -> impl Iterator<Item = &'static str> {
    SALES_LIMIT_FIELDS
        .iter()
        .chain(TIME_LIMIT_FIELDS.iter())
        .copied()
}

/// `overrideApplies(subscription)`.
pub fn override_applies(sub: Option<&CurrentSub>) -> bool {
    let Some(s) = sub else { return false };
    matches!(s.status.as_str(), "ACTIVE" | "TRIAL")
        || (s.billing_source == "MANUAL" && s.status == "INACTIVE")
}

// ── DTO ─────────────────────────────────────────────────────────────────────

/// `(field, min, max)` kolom cacahan (`OverrideLimitValueRule`).
const LIMIT_RANGES: [(&str, i64, i64); 9] = [
    ("maxStores", 1, 1_000_000),
    ("totalGeofence", 1, 1_000_000),
    ("maxAutoCheckIns", 1, 1_000_000),
    ("maxFormTemplates", 1, 1_000_000),
    ("visitHistoryDays", 1, 1_000_000),
    ("maxProject", 1, 1_000_000),
    ("dataHistoryMonths", 1, 120),
    ("reportExportsPerMonth", 1, 100_000),
    ("screenshotRetentionDays", 1, 36_499),
];

const BOOL_FIELDS: [&str; 7] = [
    "auditLog",
    "form",
    "timelineRoute",
    "product",
    "order",
    "divisions",
    "clientProofExportAllowed",
];

/// Urutan deklarasi `PutPlanOverrideDto`.
const DTO_FIELDS: [&str; 20] = [
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
    "maxProject",
    "dataHistoryMonths",
    "reportExportsPerMonth",
    "screenshotRetentionDays",
    "screenshotResolution",
    "budgetAlertDeliveryChannel",
    "clientProofExportAllowed",
    "note",
    "reason",
];

fn is_resolution(s: &str) -> bool {
    let Some((w, h)) = s.split_once('x') else {
        return false;
    };
    let ok = |p: &str| (2..=5).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit());
    s.len() <= 20 && ok(w) && ok(h)
}

/// `PutPlanOverrideDto` di bawah `ADMIN_VALIDATION_PIPE`. Hasil: body ter-trim.
pub fn validate_put(body: &Value) -> ApiResult<Map<String, Value>> {
    let mut m = as_obj(body);
    trim_field(&mut m, "note");
    trim_field(&mut m, "reason");
    let mut v = V::new();
    for e in array_keys(body) {
        v.push(e);
    }
    v.whitelist(&m, &DTO_FIELDS);
    for field in DTO_FIELDS {
        let val = m.get(field);
        if let Some((_, min, max)) = LIMIT_RANGES.iter().find(|(f, _, _)| *f == field) {
            let ok = absent(val)
                || val.and_then(Value::as_str) == Some(UNLIMITED)
                || num_of(val)
                    .is_some_and(|n| n.fract() == 0.0 && n >= *min as f64 && n <= *max as f64);
            if !ok {
                v.push(format!(
                    "{field} must be null, \"{UNLIMITED}\", or an integer between {min} and {max}"
                ));
            }
        } else if BOOL_FIELDS.contains(&field) {
            if !(absent(val) || matches!(val, Some(Value::Bool(_)))) {
                v.push(format!("{field} must be null or a boolean"));
            }
        } else if field == "screenshotResolution" {
            if !(absent(val) || matches!(val, Some(Value::String(s)) if is_resolution(s))) {
                v.push(format!(
                    "{field} must be null or a resolution like 1920x1080"
                ));
            }
        } else if field == "budgetAlertDeliveryChannel" {
            if !(absent(val)
                || matches!(
                    val.and_then(Value::as_str),
                    Some("INAPP_ONLY" | "EMAIL_SLACK")
                ))
            {
                v.push(format!(
                    "{field} must be null or one of: INAPP_ONLY, EMAIL_SLACK"
                ));
            }
        } else if field == "note" {
            if !absent(val) {
                v.max_length("note", val, 1000);
                v.is_string("note", val);
            }
        } else if field == "reason" {
            v.reason(val);
        }
    }
    v.finish()?;
    Ok(m)
}

/// `toOverrideData(dto)`: `undefined → null`, `"UNLIMITED"` → konvensi kolom.
pub fn to_override_data(dto: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for field in override_limit_fields() {
        let value = dto.get(field).cloned().unwrap_or(Value::Null);
        let value = if unlimited_of(field).is_some() {
            resolve_override_value(field, &value)
        } else {
            value
        };
        out.insert(field.to_owned(), value);
    }
    out.insert(
        "note".into(),
        dto.get("note").cloned().unwrap_or(Value::Null),
    );
    out
}

// ── baca ────────────────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct OverrideRow {
    cols: SqlJson<Value>,
    #[sqlx(rename = "createdAt")]
    created_at: NaiveDateTime,
    #[sqlx(rename = "updatedAt")]
    updated_at: NaiveDateTime,
    op_uuid: Option<uuid::Uuid>,
    op_name: Option<String>,
}

async fn read_override(
    conn: &mut PgConnection,
    company_id: i32,
) -> sqlx::Result<Option<OverrideRow>> {
    sqlx::query_as(&format!(
        r#"SELECT {OVERRIDE_JSON} AS cols, po."createdAt", po."updatedAt", o.uuid AS op_uuid, o.name AS op_name
           FROM "companyPlanOverride" po LEFT JOIN operators o ON o.id = po."updatedByOperatorId"
           WHERE po."companyId" = $1"#
    ))
    .bind(company_id)
    .fetch_optional(conn)
    .await
}

/// `toView(row)`: nilai tanpa batas → `"UNLIMITED"`.
fn to_view(row: Option<&OverrideRow>) -> Value {
    let Some(r) = row else { return Value::Null };
    let cols = obj(r.cols.0.clone());
    let mut out = Map::new();
    for field in override_limit_fields() {
        let value = cols.get(field).cloned().unwrap_or(Value::Null);
        let unlimited = unlimited_of(field);
        let value = match (unlimited, value.as_i64()) {
            (Some(u), Some(n)) if u == n => Value::String(UNLIMITED.into()),
            _ => value,
        };
        out.insert(field.to_owned(), value);
    }
    out.insert(
        "note".into(),
        cols.get("note").cloned().unwrap_or(Value::Null),
    );
    out.insert("createdAt".into(), date_of(r.created_at));
    out.insert("updatedAt".into(), date_of(r.updated_at));
    out.insert(
        "updatedBy".into(),
        match (&r.op_uuid, &r.op_name) {
            (Some(u), Some(n)) => json!({ "uuid": u.to_string(), "name": n }),
            _ => Value::Null,
        },
    );
    Value::Object(out)
}

/// `auditSnapshot(row)`: kolom batas + note (nilai mentah).
fn audit_snapshot(cols: Option<&Map<String, Value>>) -> Value {
    let Some(c) = cols else { return Value::Null };
    let mut out = Map::new();
    for field in override_limit_fields().chain(std::iter::once("note")) {
        out.insert(
            field.to_owned(),
            c.get(field).cloned().unwrap_or(Value::Null),
        );
    }
    Value::Object(out)
}

/// Node: src/admin-company/plan-override/plan-override.controller.ts:46 (`GET /admin-company/:uuid/plan-override`)
pub async fn h_get(
    State(state): State<AppState>,
    op: Op,
    audit: AuditHandle,
    Path(raw): Path<String>,
) -> ApiResult<Json<Value>> {
    gate(op, &audit, None, Some(ALL_ROLES))?;
    let uuid = uuid_pipe(&raw)?;
    let company_id: Option<i32> = sqlx::query_scalar("SELECT id FROM company WHERE uuid = $1")
        .bind(uuid)
        .fetch_optional(&state.db)
        .await?;
    let Some(company_id) = company_id else {
        return Err(company_not_found());
    };
    let mut conn = state.db.acquire().await?;
    let row = read_override(&mut conn, company_id).await?;
    let sub = current_sub(&mut *conn, company_id).await?;
    Ok(Json(ok(json!({
        "override": to_view(row.as_ref()),
        "applies": override_applies(sub.as_ref()),
    }))))
}

fn opt_i32(v: Option<&Value>) -> Option<i32> {
    v.and_then(Value::as_f64).map(|n| n as i32)
}

/// Node: src/admin-company/plan-override/plan-override.controller.ts:57 (`PUT /admin-company/:uuid/plan-override`)
pub async fn h_put(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw): Path<String>,
    body: RawBody,
) -> ApiResult<Json<Value>> {
    let op = gate(op, &audit, Some(&body), Some(OWNER_FINANCE))?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let dto = validate_put(&body_value(body))?;
    let uuid = uuid_pipe(&raw)?;
    let reason = dto
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let data = to_override_data(&dto);

    let mut tx = state.db.begin().await?;
    let Some(company_id) = lock_company_by_uuid(&mut tx, &uuid).await? else {
        return Err(company_not_found());
    };
    let applies = override_applies(current_sub(&mut *tx, company_id).await?.as_ref());
    let before = read_override(&mut tx, company_id).await?;

    let now = now_naive();
    let g = |k: &str| data.get(k);
    let b = |k: &str| data.get(k).and_then(Value::as_bool);
    sqlx::query(
        r#"INSERT INTO "companyPlanOverride" ("companyId", "maxStores", "totalGeofence", "maxAutoCheckIns",
               "maxFormTemplates", "visitHistoryDays", "auditLog", form, "timelineRoute", product, "order",
               divisions, "maxProject", "dataHistoryMonths", "reportExportsPerMonth", "screenshotRetentionDays",
               "screenshotResolution", "budgetAlertDeliveryChannel", "clientProofExportAllowed", note,
               "updatedByOperatorId", "updatedAt")
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17,
                   $18::"BudgetAlertDeliveryChannel", $19, $20, $21, $22)
           ON CONFLICT ("companyId") DO UPDATE SET
               "maxStores" = EXCLUDED."maxStores", "totalGeofence" = EXCLUDED."totalGeofence",
               "maxAutoCheckIns" = EXCLUDED."maxAutoCheckIns", "maxFormTemplates" = EXCLUDED."maxFormTemplates",
               "visitHistoryDays" = EXCLUDED."visitHistoryDays", "auditLog" = EXCLUDED."auditLog",
               form = EXCLUDED.form, "timelineRoute" = EXCLUDED."timelineRoute", product = EXCLUDED.product,
               "order" = EXCLUDED."order", divisions = EXCLUDED.divisions, "maxProject" = EXCLUDED."maxProject",
               "dataHistoryMonths" = EXCLUDED."dataHistoryMonths",
               "reportExportsPerMonth" = EXCLUDED."reportExportsPerMonth",
               "screenshotRetentionDays" = EXCLUDED."screenshotRetentionDays",
               "screenshotResolution" = EXCLUDED."screenshotResolution",
               "budgetAlertDeliveryChannel" = EXCLUDED."budgetAlertDeliveryChannel",
               "clientProofExportAllowed" = EXCLUDED."clientProofExportAllowed", note = EXCLUDED.note,
               "updatedByOperatorId" = EXCLUDED."updatedByOperatorId", "updatedAt" = EXCLUDED."updatedAt""#,
    )
    .bind(company_id)
    .bind(opt_i32(g("maxStores")))
    .bind(opt_i32(g("totalGeofence")))
    .bind(opt_i32(g("maxAutoCheckIns")))
    .bind(opt_i32(g("maxFormTemplates")))
    .bind(opt_i32(g("visitHistoryDays")))
    .bind(b("auditLog"))
    .bind(b("form"))
    .bind(b("timelineRoute"))
    .bind(b("product"))
    .bind(b("order"))
    .bind(b("divisions"))
    .bind(opt_i32(g("maxProject")))
    .bind(opt_i32(g("dataHistoryMonths")))
    .bind(opt_i32(g("reportExportsPerMonth")))
    .bind(opt_i32(g("screenshotRetentionDays")))
    .bind(g("screenshotResolution").and_then(Value::as_str))
    .bind(g("budgetAlertDeliveryChannel").and_then(Value::as_str))
    .bind(b("clientProofExportAllowed"))
    .bind(g("note").and_then(Value::as_str))
    .bind(op.id)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    let row = read_override(&mut tx, company_id).await?;
    let applied = apply_company_plan_limits(&mut tx, &audit, company_id).await?;

    let before_v = json!({
        "override": audit_snapshot(before.as_ref().and_then(|r| r.cols.0.as_object())),
    });
    let after_v = json!({
        "override": audit_snapshot(row.as_ref().and_then(|r| r.cols.0.as_object())),
        "timeLimits": applied.map_or(Value::Null, |a| Value::Object(a.written)),
    });
    record(
        &mut *tx,
        &ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "plan_override.set",
            target_type: "company",
            target_uuid: Some(uuid),
            company_id: Some(company_id),
            before: Some(&before_v),
            after: Some(&after_v),
            reason: Some(&reason),
        },
    )
    .await?;
    tx.commit().await?;

    invalidate(&state, company_id).await;
    Ok(Json(ok(json!({
        "override": to_view(row.as_ref()),
        "applies": applies,
    }))))
}

/// Node: src/admin-company/plan-override/plan-override.controller.ts:78 (`DELETE /admin-company/:uuid/plan-override`)
pub async fn h_delete(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw): Path<String>,
    body: RawBody,
) -> ApiResult<Json<Value>> {
    let op = gate(op, &audit, Some(&body), Some(OWNER_FINANCE))?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let reason = crate::lifecycle::validate_reason_dto(&body_value(body))?;
    let uuid = uuid_pipe(&raw)?;

    let mut tx = state.db.begin().await?;
    let Some(company_id) = lock_company_by_uuid(&mut tx, &uuid).await? else {
        return Err(company_not_found());
    };
    let applies = override_applies(current_sub(&mut *tx, company_id).await?.as_ref());
    let before = read_override(&mut tx, company_id).await?;
    if before.is_some() {
        sqlx::query(r#"DELETE FROM "companyPlanOverride" WHERE "companyId" = $1"#)
            .bind(company_id)
            .execute(&mut *tx)
            .await?;
    }
    let applied = apply_company_plan_limits(&mut tx, &audit, company_id).await?;
    let before_v = json!({
        "override": audit_snapshot(before.as_ref().and_then(|r| r.cols.0.as_object())),
    });
    let after_v = json!({
        "override": Value::Null,
        "timeLimits": applied.map_or(Value::Null, |a| Value::Object(a.written)),
    });
    record(
        &mut *tx,
        &ctx,
        &OperatorAuditEntry {
            operator_id: op.id,
            action: "plan_override.reset",
            target_type: "company",
            target_uuid: Some(uuid),
            company_id: Some(company_id),
            before: Some(&before_v),
            after: Some(&after_v),
            reason: Some(&reason),
        },
    )
    .await?;
    tx.commit().await?;

    invalidate(&state, company_id).await;
    Ok(Json(ok(
        json!({ "override": Value::Null, "applies": applies }),
    )))
}

async fn invalidate(state: &AppState, company_id: i32) {
    after_company_change(
        state,
        company_id,
        CompanyChange {
            plan: true,
            user_me: true,
            divisions: true,
            modules: false,
        },
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errs(body: Value) -> Vec<String> {
        match validate_put(&body) {
            Err(ApiError::Validation(m)) => m,
            _ => Vec::new(),
        }
    }

    #[test]
    fn full_contract_example_is_valid() {
        assert!(
            errs(json!({
                "maxStores": 500, "totalGeofence": 20, "maxAutoCheckIns": "UNLIMITED",
                "maxFormTemplates": 50, "visitHistoryDays": 365, "auditLog": true, "form": true,
                "timelineRoute": null, "product": false, "order": null, "divisions": true,
                "maxProject": 100, "dataHistoryMonths": 24, "reportExportsPerMonth": "UNLIMITED",
                "screenshotRetentionDays": 365, "screenshotResolution": "1920x1080",
                "budgetAlertDeliveryChannel": "EMAIL_SLACK", "clientProofExportAllowed": true,
                "note": "Kontrak 2026", "reason": "Kesepakatan kontrak tahunan"
            }))
            .is_empty()
        );
        // field yang tidak dikirim boleh.
        assert!(errs(json!({ "reason": "hanya alasan" })).is_empty());
    }

    #[test]
    fn rejects_unlimited_sentinels_as_numbers() {
        assert_eq!(
            errs(
                json!({ "maxStores": 0, "dataHistoryMonths": -1, "screenshotRetentionDays": 36500, "reason": "valid reason" })
            ),
            vec![
                "maxStores must be null, \"UNLIMITED\", or an integer between 1 and 1000000",
                "dataHistoryMonths must be null, \"UNLIMITED\", or an integer between 1 and 120",
                "screenshotRetentionDays must be null, \"UNLIMITED\", or an integer between 1 and 36499",
            ]
        );
    }

    #[test]
    fn booleans_channel_resolution_reason_note_and_whitelist() {
        assert_eq!(
            errs(json!({
                "foo": 1, "auditLog": "yes", "budgetAlertDeliveryChannel": "SMS",
                "screenshotResolution": "1x1", "note": "x".repeat(1001), "reason": "  abc "
            })),
            vec![
                "property foo should not exist",
                "auditLog must be null or a boolean",
                "screenshotResolution must be null or a resolution like 1920x1080",
                "budgetAlertDeliveryChannel must be null or one of: INAPP_ONLY, EMAIL_SLACK",
                "note must be shorter than or equal to 1000 characters",
                "reason must be longer than or equal to 5 characters",
            ]
        );
    }

    #[test]
    fn override_data_maps_unlimited_and_missing() {
        let dto = obj(
            json!({ "maxStores": "UNLIMITED", "reportExportsPerMonth": "UNLIMITED", "form": true }),
        );
        let d = to_override_data(&dto);
        assert_eq!(d.get("maxStores"), Some(&json!(0)));
        assert_eq!(d.get("reportExportsPerMonth"), Some(&json!(-1)));
        assert_eq!(d.get("form"), Some(&json!(true)));
        assert_eq!(d.get("divisions"), Some(&Value::Null));
        assert_eq!(d.get("note"), Some(&Value::Null));
    }
}
