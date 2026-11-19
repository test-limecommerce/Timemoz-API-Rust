//! Port `src/admin-pricing/*` (`admin-pricing.controller.ts`, `admin-pricing.service.ts`).
//!
//! * `GET /admin-pricing` (`:31`): hanya `JwtAdminAuthGuard` (semua operator aktif), tanpa audit.
//! * `GET /admin-pricing/:uuid` (`:38`): `ParseUUIDPipe`; 200 bahkan untuk not-found (`success: false`).
//! * `POST /admin-pricing/create` (`:45`): 201, `@OperatorAuditAction('pricing.create')`.
//! * `PATCH /admin-pricing/update/:uuid` (`:53`): 200, audit `pricing.update` (targetParam uuid).
//! * `DELETE /admin-pricing/delete/:uuid` (`:67`): 200, audit `pricing.delete` (targetParam uuid).
//!
//! Semua body/galat service memakai bentuk `{ success, message, data }`. Audit hanya bila `success`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use mz_core::prisma::parse_uuid;
use mz_core::time::JsDate;
use mz_core::uuid_rules::parse_uuid_pipe;
use mz_core::{ApiResult, AppState};
use mz_operator::{AuditCtx, AuditMeta, Operator, audit_after_success};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::common::{
    ListQuery, RawBody, RawQuery, db_fail, fail, field, finish, int_or, int32, is_success, js_f64,
    ok, opt_bool, opt_number, opt_one_of, opt_string, parse_list_query, req_number, req_string,
    skip_for, str_of, updatable,
};

/// `AdminListQueryDto` subclass `PricingListQueryDto` (`PRICING_ORDER_COLUMNS`).
const ORDER_COLUMNS: &[&str] = &["createdAt", "name", "basePrice", "pricePerUserUsd", "id"];

const CREATE_META: AuditMeta = AuditMeta {
    action: "pricing.create",
    target_type: "pricing",
    company_from: None,
};
const UPDATE_META: AuditMeta = AuditMeta {
    action: "pricing.update",
    target_type: "pricing",
    company_from: None,
};
const DELETE_META: AuditMeta = AuditMeta {
    action: "pricing.delete",
    target_type: "pricing",
    company_from: None,
};

/// Kolom `pricingTiers` (`select` tanpa `select` di Prisma = semua skalar, urutan skema).
const ROW_COLS: &str = r#"id, uuid::text AS "uuid", name, "minUsers", "maxUsers", "basePrice",
    "extraUserPrice", "pricePerUserUsd", "billingInterval", "productType"::text AS "productType",
    "paypalPlanId", "paypalProductId", "createdAt", "updatedAt", status, "maxProject",
    "maxAppUrlPerUser", "useIdleDetection", "useCheatDetection", "useReports",
    "useTimesheetApproval", "useActivityLevel", "useIntegrations", "screenshotResolution",
    "screenshotRetentionDays", "reportExportsPerMonth", "dataHistoryMonths",
    "budgetAlertDeliveryChannel"::text AS "budgetAlertDeliveryChannel",
    "clientProofExportAllowed", "maxStores", "totalGeofence", "auditLog", divisions, form,
    "timelineRoute", product, "order", "maxAutoCheckIns", "maxFormTemplates", "visitHistoryDays",
    "listFeatures""#;

const BUDGET_CHANNELS: &[&str] = &["INAPP_ONLY", "EMAIL_SLACK"];

#[derive(Debug, sqlx::FromRow, Serialize)]
#[sqlx(rename_all = "camelCase")]
#[serde(rename_all = "camelCase")]
struct PricingRow {
    id: i32,
    uuid: String,
    name: String,
    min_users: i32,
    max_users: i32,
    #[serde(serialize_with = "js_f64")]
    base_price: f64,
    #[serde(serialize_with = "js_f64")]
    extra_user_price: f64,
    #[serde(serialize_with = "js_f64")]
    price_per_user_usd: f64,
    billing_interval: String,
    product_type: String,
    paypal_plan_id: Option<String>,
    paypal_product_id: Option<String>,
    created_at: JsDate,
    updated_at: Option<JsDate>,
    status: i32,
    max_project: i32,
    max_app_url_per_user: i32,
    use_idle_detection: Option<bool>,
    use_cheat_detection: Option<bool>,
    use_reports: Option<bool>,
    use_timesheet_approval: Option<bool>,
    use_activity_level: Option<bool>,
    use_integrations: Option<bool>,
    screenshot_resolution: String,
    screenshot_retention_days: i32,
    report_exports_per_month: i32,
    data_history_months: i32,
    budget_alert_delivery_channel: String,
    client_proof_export_allowed: bool,
    max_stores: i32,
    total_geofence: i32,
    audit_log: bool,
    divisions: bool,
    form: bool,
    timeline_route: bool,
    product: bool,
    order: bool,
    max_auto_check_ins: i32,
    max_form_templates: i32,
    visit_history_days: i32,
    list_features: Option<Value>,
}

/// `select` `findOne`: id, uuid, name, minUsers, maxUsers, basePrice, extraUserPrice, status,
/// listFeatures, createdAt, updatedAt.
#[derive(Debug, sqlx::FromRow, Serialize)]
#[sqlx(rename_all = "camelCase")]
#[serde(rename_all = "camelCase")]
struct PricingDetail {
    id: i32,
    uuid: String,
    name: String,
    min_users: i32,
    max_users: i32,
    #[serde(serialize_with = "js_f64")]
    base_price: f64,
    #[serde(serialize_with = "js_f64")]
    extra_user_price: f64,
    status: i32,
    list_features: Option<Value>,
    created_at: JsDate,
    updated_at: Option<JsDate>,
}

/// Kolom Float Prisma: `-0` dari JSON tersimpan `0` di Node (paritas adm-misc), Rust menyimpan
/// `-0` apa adanya. `-0.0 + 0.0 == +0.0` (IEEE 754).
fn js_float(n: f64) -> f64 {
    n + 0.0
}

/// Filter `search` (`contains`, insensitive) pada `name`. Prisma `contains` + `insensitive` =
/// `ILIKE '%' || $1 || '%'` TANPA meng-escape `%`/`_` (paritas adm-misc: `search=%` = semua baris).
fn push_search(qb: &mut QueryBuilder<'_, Postgres>, search: Option<&str>) {
    if let Some(s) = search.filter(|s| !s.is_empty()) {
        qb.push(" WHERE name ILIKE '%' || ")
            .push_bind(s.to_owned())
            .push(" || '%'");
    }
}

/// Node: src/admin-pricing/admin-pricing.controller.ts:31 (`findAll`).
pub async fn h_list(
    State(state): State<AppState>,
    _op: Operator,
    RawQuery(raw): RawQuery,
) -> ApiResult<Json<Value>> {
    let q: ListQuery = parse_list_query(raw.as_deref(), ORDER_COLUMNS)?;
    let skip = skip_for(q.page, q.per_page)?;

    let mut count = QueryBuilder::<Postgres>::new(r#"SELECT COUNT(*) FROM "pricingTiers""#);
    push_search(&mut count, q.search.as_deref());
    let total: i64 = count.build_query_scalar().fetch_one(&state.db).await?;

    let mut qb = QueryBuilder::<Postgres>::new(format!(r#"SELECT {ROW_COLS} FROM "pricingTiers""#));
    push_search(&mut qb, q.search.as_deref());
    qb.push(" ORDER BY \"")
        .push(q.order_column)
        .push(if q.order_asc { "\" ASC" } else { "\" DESC" });
    qb.push(" OFFSET ").push_bind(skip);
    qb.push(" LIMIT ").push_bind(q.per_page);
    let rows: Vec<PricingRow> = qb.build_query_as().fetch_all(&state.db).await?;

    Ok(Json(ok(
        "Success get all data pricing tiers",
        json!({
            "total": total,
            "itemsPerPage": q.per_page,
            "pageNumber": q.page,
            "result": rows,
        }),
    )))
}

/// Node: src/admin-pricing/admin-pricing.controller.ts:38 (`findOne`, `ParseUUIDPipe`).
pub async fn h_find_one(
    State(state): State<AppState>,
    _op: Operator,
    Path(uuid): Path<String>,
) -> ApiResult<Json<Value>> {
    parse_uuid_pipe(&uuid)?;
    Ok(Json(find_one(&state, &uuid).await))
}

/// `GET /admin-pricing/create`: Nest mencocokkannya ke `GET :uuid` (ParseUUIDPipe → 400).
pub async fn h_find_literal_create(
    State(state): State<AppState>,
    _op: Operator,
) -> ApiResult<Json<Value>> {
    parse_uuid_pipe("create")?;
    Ok(Json(find_one(&state, "create").await))
}

async fn find_one(state: &AppState, uuid: &str) -> Value {
    const MSG: &str = "Failed to get pricing details";
    let Ok(id) = parse_uuid(uuid) else {
        return fail(MSG);
    };
    let row: Result<Option<PricingDetail>, sqlx::Error> = sqlx::query_as(
        r#"SELECT id, uuid::text AS "uuid", name, "minUsers", "maxUsers", "basePrice",
                  "extraUserPrice", status, "listFeatures", "createdAt", "updatedAt"
             FROM "pricingTiers" WHERE uuid = $1 LIMIT 1"#,
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await;
    match row {
        Ok(Some(p)) => ok(
            "Success get pricing details",
            serde_json::to_value(p).unwrap_or(Value::Null),
        ),
        Ok(None) => fail("Pricing not found"),
        Err(e) => db_fail("pricing.find_one", MSG, &e),
    }
}

/// Validasi `CreatePricingDto` (`ValidationPipe` global: tanpa whitelist, properti asing diabaikan).
/// Dipakai create dan update (DTO yang sama).
fn validate_pricing(body: &Value) -> ApiResult<()> {
    let mut e = Vec::new();
    req_string(&mut e, body, "name");
    for k in [
        "minUsers",
        "maxUsers",
        "basePrice",
        "extraUserPrice",
        "status",
    ] {
        req_number(&mut e, body, k);
    }
    opt_string(&mut e, body, "screenshotResolution");
    for k in [
        "screenshotRetentionDays",
        "reportExportsPerMonth",
        "dataHistoryMonths",
    ] {
        opt_number(&mut e, body, k);
    }
    opt_one_of(&mut e, body, "budgetAlertDeliveryChannel", BUDGET_CHANNELS);
    opt_bool(&mut e, body, "clientProofExportAllowed");
    finish(e)
}

const CREATE_FAIL: &str = "Failed to create pricing";

/// Node: src/admin-pricing/admin-pricing.controller.ts:45 (`createPricingTiers`, 201).
pub async fn h_create(
    State(state): State<AppState>,
    op: Operator,
    audit: AuditCtx,
    RawBody(body): RawBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    validate_pricing(&body)?;
    let out = match create(&state, &body).await {
        Ok(v) => v,
        Err(e) => db_fail("pricing.create", CREATE_FAIL, &e),
    };
    if is_success(&out) {
        audit_after_success(&state, &op, &audit, CREATE_META, None, Some(&body)).await;
    }
    Ok((StatusCode::CREATED, Json(out)))
}

/// `service.create`: cek nama unik, lalu INSERT dengan default. Galat konversi Int = galat Prisma.
async fn create(state: &AppState, body: &Value) -> Result<Value, sqlx::Error> {
    let name = str_of(body, "name").unwrap_or_default();
    let dup: Option<i32> =
        sqlx::query_scalar(r#"SELECT id FROM "pricingTiers" WHERE name = $1 LIMIT 1"#)
            .bind(name)
            .fetch_optional(&state.db)
            .await?;
    if dup.is_some() {
        return Ok(fail("Pricing with this name already exists"));
    }
    let num = |k: &str| field(body, k).and_then(Value::as_f64).and_then(int32);
    let (Some(min_users), Some(max_users), Some(status)) =
        (num("minUsers"), num("maxUsers"), num("status"))
    else {
        return Ok(fail(CREATE_FAIL));
    };
    let base = field(body, "basePrice")
        .and_then(Value::as_f64)
        .map(js_float)
        .unwrap_or_default();
    let extra = field(body, "extraUserPrice")
        .and_then(Value::as_f64)
        .map(js_float)
        .unwrap_or_default();
    let list = match field(body, "listFeatures") {
        None | Some(Value::Null) => json!([]),
        Some(v) => v.clone(),
    };
    let resolution = str_of(body, "screenshotResolution").unwrap_or("640x360");
    let (Some(retention), Some(reports), Some(history)) = (
        int_or(body, "screenshotRetentionDays", 7),
        int_or(body, "reportExportsPerMonth", 3),
        int_or(body, "dataHistoryMonths", 3),
    ) else {
        return Ok(fail(CREATE_FAIL));
    };
    let channel = str_of(body, "budgetAlertDeliveryChannel").unwrap_or("INAPP_ONLY");
    let proof = field(body, "clientProofExportAllowed")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let (id, uuid): (i32, String) = sqlx::query_as(
        r#"INSERT INTO "pricingTiers" (name, "minUsers", "maxUsers", "basePrice", "extraUserPrice",
                status, "listFeatures", "screenshotResolution", "screenshotRetentionDays",
                "reportExportsPerMonth", "dataHistoryMonths", "budgetAlertDeliveryChannel",
                "clientProofExportAllowed")
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12::"BudgetAlertDeliveryChannel", $13)
           RETURNING id, uuid::text"#,
    )
    .bind(name)
    .bind(min_users)
    .bind(max_users)
    .bind(base)
    .bind(extra)
    .bind(status)
    .bind(sqlx::types::Json(list))
    .bind(resolution)
    .bind(retention)
    .bind(reports)
    .bind(history)
    .bind(channel)
    .bind(proof)
    .fetch_one(&state.db)
    .await?;
    Ok(ok(
        "Successfully created pricing",
        json!({ "id": id, "uuid": uuid }),
    ))
}

/// Node: src/admin-pricing/admin-pricing.controller.ts:53 (`updatePricingTiers`, 200).
pub async fn h_update(
    State(state): State<AppState>,
    op: Operator,
    audit: AuditCtx,
    Path(uuid): Path<String>,
    RawBody(body): RawBody,
) -> ApiResult<Json<Value>> {
    validate_pricing(&body)?;
    let out = match update(&state, &uuid, &body).await {
        Ok(v) => v,
        Err(e) => db_fail("pricing.update", "Failed to update pricing", &e),
    };
    if is_success(&out) {
        audit_after_success(&state, &op, &audit, UPDATE_META, Some(&uuid), Some(&body)).await;
    }
    Ok(Json(out))
}

const UPDATE_FAIL: &str = "Failed to update pricing";

/// `service.update`. `findFirst({ uuid, name })` lalu `updateMany({ uuid })`: 0 baris tetap sukses.
async fn update(state: &AppState, uuid: &str, body: &Value) -> Result<Value, sqlx::Error> {
    // uuid tak valid → Prisma melempar P2023 → catch Node.
    let Ok(uuid_val) = parse_uuid(uuid) else {
        return Ok(fail(UPDATE_FAIL));
    };
    let name = str_of(body, "name").unwrap_or_default();
    let found: Option<String> = sqlx::query_scalar(
        r#"SELECT uuid::text FROM "pricingTiers" WHERE uuid = $1 AND name = $2 LIMIT 1"#,
    )
    .bind(uuid_val)
    .bind(name)
    .fetch_optional(&state.db)
    .await?;
    // Perbandingan dengan string parameter apa adanya (Node: `pricing.uuid !== uuid`).
    if found.is_some_and(|u| u != uuid) {
        return Ok(fail("Pricing with this name already exists"));
    }

    let num = |k: &str| field(body, k).and_then(Value::as_f64).and_then(int32);
    let (Some(min_users), Some(max_users), Some(status)) =
        (num("minUsers"), num("maxUsers"), num("status"))
    else {
        return Ok(fail(UPDATE_FAIL));
    };
    let base = field(body, "basePrice")
        .and_then(Value::as_f64)
        .map(js_float)
        .unwrap_or_default();
    let extra = field(body, "extraUserPrice")
        .and_then(Value::as_f64)
        .map(js_float)
        .unwrap_or_default();

    // Field opsional: hanya bila `!== undefined`; `null` = galat Prisma.
    let (
        Some(list),
        Some(resolution),
        Some(retention),
        Some(reports),
        Some(history),
        Some(channel),
        Some(proof),
    ) = (
        updatable(body, "listFeatures"),
        updatable(body, "screenshotResolution"),
        updatable(body, "screenshotRetentionDays"),
        updatable(body, "reportExportsPerMonth"),
        updatable(body, "dataHistoryMonths"),
        updatable(body, "budgetAlertDeliveryChannel"),
        updatable(body, "clientProofExportAllowed"),
    )
    else {
        return Ok(fail(UPDATE_FAIL));
    };
    let as_int = |v: Option<&Value>| match v {
        None => Some(None),
        Some(v) => v.as_f64().and_then(int32).map(Some),
    };
    let (Some(retention), Some(reports), Some(history)) =
        (as_int(retention), as_int(reports), as_int(history))
    else {
        return Ok(fail(UPDATE_FAIL));
    };

    let mut qb = QueryBuilder::<Postgres>::new(r#"UPDATE "pricingTiers" SET "#);
    {
        let mut sep = qb.separated(", ");
        sep.push("name = ").push_bind_unseparated(name.to_owned());
        sep.push("\"minUsers\" = ").push_bind_unseparated(min_users);
        sep.push("\"maxUsers\" = ").push_bind_unseparated(max_users);
        sep.push("\"basePrice\" = ").push_bind_unseparated(base);
        sep.push("\"extraUserPrice\" = ")
            .push_bind_unseparated(extra);
        sep.push("status = ").push_bind_unseparated(status);
        if let Some(v) = list {
            sep.push("\"listFeatures\" = ")
                .push_bind_unseparated(sqlx::types::Json(v.clone()));
        }
        if let Some(v) = resolution.and_then(Value::as_str) {
            sep.push("\"screenshotResolution\" = ")
                .push_bind_unseparated(v.to_owned());
        }
        if let Some(v) = retention {
            sep.push("\"screenshotRetentionDays\" = ")
                .push_bind_unseparated(v);
        }
        if let Some(v) = reports {
            sep.push("\"reportExportsPerMonth\" = ")
                .push_bind_unseparated(v);
        }
        if let Some(v) = history {
            sep.push("\"dataHistoryMonths\" = ")
                .push_bind_unseparated(v);
        }
        if let Some(v) = channel.and_then(Value::as_str) {
            sep.push("\"budgetAlertDeliveryChannel\" = ")
                .push_bind_unseparated(v.to_owned())
                .push_unseparated("::\"BudgetAlertDeliveryChannel\"");
        }
        if let Some(v) = proof.and_then(Value::as_bool) {
            sep.push("\"clientProofExportAllowed\" = ")
                .push_bind_unseparated(v);
        }
    }
    qb.push(" WHERE uuid = ").push_bind(uuid_val);
    qb.build().execute(&state.db).await?;
    Ok(ok("Successfully updated pricing", json!({ "uuid": uuid })))
}

/// Node: src/admin-pricing/admin-pricing.controller.ts:67 (`deletePricingTiers`, 200).
pub async fn h_delete(
    State(state): State<AppState>,
    op: Operator,
    audit: AuditCtx,
    Path(uuid): Path<String>,
) -> ApiResult<Json<Value>> {
    let out = match delete(&state, &uuid).await {
        Ok(v) => v,
        Err(e) => db_fail("pricing.delete", "Failed to delete pricing", &e),
    };
    if is_success(&out) {
        audit_after_success(&state, &op, &audit, DELETE_META, Some(&uuid), None).await;
    }
    Ok(Json(out))
}

/// `service.delete`: tolak bila masih ada `subscriptions` yang memakai paket ini.
async fn delete(state: &AppState, uuid: &str) -> Result<Value, sqlx::Error> {
    const MSG: &str = "Failed to delete pricing";
    let Ok(uuid_val) = parse_uuid(uuid) else {
        return Ok(fail(MSG));
    };
    let id: Option<i32> =
        sqlx::query_scalar(r#"SELECT id FROM "pricingTiers" WHERE uuid = $1 LIMIT 1"#)
            .bind(uuid_val)
            .fetch_optional(&state.db)
            .await?;
    let Some(id) = id else {
        return Ok(fail("Pricing not found"));
    };
    let used: Option<i32> =
        sqlx::query_scalar(r#"SELECT id FROM subscriptions WHERE "pricingTierId" = $1 LIMIT 1"#)
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    if used.is_some() {
        return Ok(fail(
            "Cannot delete pricing tier associated with existing subscriptions",
        ));
    }
    sqlx::query(r#"DELETE FROM "pricingTiers" WHERE uuid = $1"#)
        .bind(uuid_val)
        .execute(&state.db)
        .await?;
    Ok(ok("Successfully deleted pricing", Value::Null))
}
