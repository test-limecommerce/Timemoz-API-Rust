//! `src/admin-company/admin-company.controller.ts` + `admin-company.service.ts` (8 route).
//!
//! Controller lama: `JwtAdminAuthGuard` per method TANPA `OperatorRolesGuard`, pipe global
//! (`ValidationPipe({ transform: true })`, tanpa whitelist). Lima PATCH ber-`@OperatorAuditAction`
//! (baris `operatorAuditLog` sesudah sukses, dilewati bila respons `success: false`).

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use chrono::{NaiveDateTime, NaiveTime};
use mz_audit::{AuditHandle, Lookup, Model, Row};
use mz_core::jsfmt::parse_int;
use mz_core::qs::QVal;
use mz_core::time::{now_naive, parse_js_date};
use mz_core::{ApiError, ApiResult, AppState};
use mz_operator::{AuditCtx, AuditMeta, CompanyFrom, Operator, audit_after_success};
use mz_paypal::PaypalClient;
use serde_json::{Map, Value, json};
use sqlx::types::Json as SqlJson;

use crate::common::{
    CompanyWhere, RawBody, body_value, clear_user_me_cache, coded, date, date_of, db_uuid, fail,
    gate, num, uuid_pipe,
};
use crate::plan_limits::{
    Bind, BoostStatusRow, binds_of, current_sub, is_paid_tier, obj, paid_tier_company_limits,
    reapply_plan_override_after_tier_sync, set_boost, to_plan_boost_status, update_company,
};
use crate::val::{V, as_obj, str_of};

type Op = Result<Operator, ApiError>;

const ORDER_COLUMNS: [&str; 4] = ["id", "name", "createdAt", "updatedAt"];

// ── GET /admin-company ──────────────────────────────────────────────────────

/// `PaginatorDto` di bawah pipe global. `Ok(None)` untuk angka = tidak dikirim.
struct ListQuery {
    page_number: Option<i64>,
    items_per_page: Option<i64>,
    order_by: Option<String>,
    order_column: Option<String>,
    search: Option<String>,
}

fn parse_list_query(raw: Option<&str>) -> ApiResult<ListQuery> {
    let map = mz_core::qs::parse(raw.unwrap_or_default());
    let get = |k: &str| map.iter().find(|(key, _)| key == k).map(|(_, v)| v);
    let mut v = V::new();
    let mut number = |k: &str| -> Option<i64> {
        match get(k) {
            None => None,
            Some(QVal::One(s)) => {
                let n = parse_int(s);
                if n.is_none() {
                    v.push(format!(
                        "{k} must be a number conforming to the specified constraints"
                    ));
                }
                n
            }
            Some(_) => {
                v.push(format!(
                    "{k} must be a number conforming to the specified constraints"
                ));
                None
            }
        }
    };
    let page_number = number("pageNumber");
    let items_per_page = number("itemsPerPage");
    let mut string = |k: &str| -> Option<String> {
        match get(k) {
            None => None,
            Some(QVal::One(s)) => Some(s.clone()),
            Some(_) => {
                v.push(format!("{k} must be a string"));
                None
            }
        }
    };
    let order_by = string("orderBy");
    let order_column = string("orderColumn");
    let search = string("search");
    v.finish()?;
    Ok(ListQuery {
        page_number,
        items_per_page,
        order_by,
        order_column,
        search,
    })
}

/// Prisma `contains` + `mode: insensitive` tidak meng-escape `%`/`_` (dibaca sebagai wildcard),
/// sama dengan modul lain (`'%' || q || '%'`).
fn like_pattern(search: &str) -> String {
    format!("%{search}%")
}

#[derive(sqlx::FromRow)]
struct ListRow {
    uuid: uuid::Uuid,
    name: String,
    telp: Option<String>,
    #[sqlx(rename = "timeZone")]
    time_zone: String,
    #[sqlx(rename = "isAgencyMode")]
    is_agency_mode: bool,
    #[sqlx(rename = "isTrial")]
    is_trial: bool,
    status: i32,
    register_name: Option<String>,
    register_email: Option<String>,
    owner_name: Option<String>,
    owner_username: Option<String>,
    #[sqlx(rename = "createdAt")]
    created_at: NaiveDateTime,
    total: i64,
    boosts: SqlJson<Vec<BoostStatusRow>>,
    has_sub: Option<bool>,
    sub_status: Option<String>,
    sub_end: Option<NaiveDateTime>,
    tier: Option<SqlJson<Value>>,
}

const BOOSTS_SQL: &str = r#"COALESCE((SELECT json_agg(json_build_object('product', pb.product::text,
        'enabled', pb.enabled, 'enabledAt', pb."enabledAt", 'disabledAt', pb."disabledAt") ORDER BY pb.id)
      FROM "companyPlanBoost" pb WHERE pb."companyId" = c.id), '[]'::json)"#;

/// Node: src/admin-company/admin-company.controller.ts:49 (`GET /admin-company`)
pub async fn h_list(
    State(state): State<AppState>,
    op: Op,
    audit: AuditHandle,
    RawQuery(raw): RawQuery,
) -> ApiResult<Json<Value>> {
    gate(op, &audit, None, None)?;
    let q = parse_list_query(raw.as_deref())?;
    let pattern = q
        .search
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(like_pattern);

    let total: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM company c
           WHERE ($1::text IS NULL OR c.name ILIKE $1 OR c.telp ILIKE $1)"#,
    )
    .bind(pattern.as_deref())
    .fetch_one(&state.db)
    .await?;

    let falsy_num = |n: Option<i64>| n.is_none_or(|x| x == 0);
    let falsy_str = |s: &Option<String>| s.as_deref().is_none_or(str::is_empty);
    let (page, items, order_by, order_column) = if falsy_num(q.page_number)
        && falsy_num(q.items_per_page)
        && falsy_str(&q.order_by)
        && falsy_str(&q.order_column)
    {
        (
            Some(1),
            Some(total),
            Some("desc".to_owned()),
            Some("createdAt".to_owned()),
        )
    } else {
        (q.page_number, q.items_per_page, q.order_by, q.order_column)
    };
    let column = order_column
        .as_deref()
        .filter(|c| ORDER_COLUMNS.contains(c))
        .unwrap_or("createdAt");
    let asc = order_by.as_deref() == Some("asc");

    // `take: NaN` / `skip: NaN` / skip negatif → galat validasi Prisma (500).
    let (Some(take), Some(page)) = (items, page) else {
        return Err(ApiError::internal(anyhow::anyhow!(
            "Invalid value for argument `take`/`skip`: NaN"
        )));
    };
    let skip = (page - 1) * take;
    if skip < 0 {
        return Err(ApiError::internal(anyhow::anyhow!(
            "Invalid value for argument `skip`: must be positive"
        )));
    }
    // `take` negatif Prisma = |take| baris dari ujung urutan (urutan tetap).
    let reverse = take < 0;
    let dir = if asc != reverse { "ASC" } else { "DESC" };

    let sql = format!(
        r#"SELECT c.uuid, c.name, c.telp, c."timeZone", c."isAgencyMode", c."isTrial", c.status,
                  c.register_name, c.register_email, ou.name AS owner_name, ou.username AS owner_username,
                  c."createdAt",
                  (SELECT COUNT(*) FROM "userCompany" uc WHERE uc."companyId" = c.id AND uc.status = 1) AS total,
                  {BOOSTS_SQL} AS boosts,
                  sub.has_sub, sub.status AS sub_status, sub."endDate" AS sub_end, sub.tier
           FROM company c
           LEFT JOIN users ou ON ou.id = c."ownerUserId"
           LEFT JOIN LATERAL (
             SELECT true AS has_sub, s.status::text AS status, s."endDate",
                    CASE WHEN pt.id IS NULL THEN NULL ELSE json_build_object('name', pt.name,
                      'productType', pt."productType"::text, 'basePrice', pt."basePrice",
                      'pricePerUserUsd', pt."pricePerUserUsd") END AS tier
             FROM subscriptions s LEFT JOIN "pricingTiers" pt ON pt.id = s."pricingTierId"
             WHERE s."companyId" = c.id AND s.status::text IN ('ACTIVE', 'TRIAL')
             ORDER BY s."startDate" DESC, s.id DESC LIMIT 1
           ) sub ON true
           WHERE ($1::text IS NULL OR c.name ILIKE $1 OR c.telp ILIKE $1)
           ORDER BY c."{column}" {dir}
           LIMIT $2 OFFSET $3"#
    );
    let mut rows: Vec<ListRow> = sqlx::query_as(&sql)
        .bind(pattern.as_deref())
        .bind(take.abs())
        .bind(skip)
        .fetch_all(&state.db)
        .await?;
    if reverse {
        rows.reverse();
    }
    let result: Vec<Value> = rows.into_iter().map(list_item).collect();
    Ok(Json(json!({
        "success": true,
        "message": "Success get all data company",
        "data": {
            "total": total,
            "itemsPerPage": take,
            "pageNumber": page,
            "result": result,
        },
    })))
}

fn list_item(r: ListRow) -> Value {
    let tier = r.tier.as_ref().and_then(|t| t.0.as_object());
    let paid = is_paid_tier(tier);
    let subscription = if r.has_sub.unwrap_or(false) {
        json!({
            "name": tier.and_then(|t| t.get("name")).cloned().unwrap_or(Value::Null),
            "type": if paid { "PRO" } else { "FREE" },
            "status": r.sub_status,
            "expiredAt": if paid { date(r.sub_end) } else { Value::Null },
        })
    } else {
        Value::Null
    };
    json!({
        "uuid": r.uuid.to_string(),
        "name": r.name,
        "telp": r.telp,
        "timeZone": r.time_zone,
        "isAgencyMode": r.is_agency_mode,
        "isTrial": r.is_trial,
        "status": r.status,
        "email": r.owner_username.or(r.register_email),
        "ownerName": r.owner_name.or(r.register_name),
        "totalEmployees": r.total,
        "createdAt": date_of(r.created_at),
        "pricingType": tier.and_then(|t| t.get("productType")).cloned().unwrap_or(Value::Null),
        "subscription": subscription,
        "planBoost": to_plan_boost_status(&r.boosts.0),
    })
}

// ── GET /admin-company/{uuid} ───────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct DetailRow {
    id: i32,
    uuid: uuid::Uuid,
    name: String,
    telp: Option<String>,
    #[sqlx(rename = "timeZone")]
    time_zone: String,
    #[sqlx(rename = "dailyReportSentAt")]
    daily_report_sent_at: NaiveTime,
    status: i32,
    #[sqlx(rename = "isAgencyMode")]
    is_agency_mode: bool,
    #[sqlx(rename = "isTrial")]
    is_trial: bool,
    #[sqlx(rename = "isAlllowDeleteScreenshot")]
    is_allow_delete_screenshot: bool,
    #[sqlx(rename = "isSupportOfflineMode")]
    is_support_offline_mode: bool,
    #[sqlx(rename = "useActivityTrack")]
    use_activity_track: bool,
    #[sqlx(rename = "fixedHourNotification")]
    fixed_hour_notification: i32,
    boosts: SqlJson<Vec<BoostStatusRow>>,
}

/// Kolom `@db.Time` Prisma = `Date` 1970-01-01.
fn time_of_day(t: NaiveTime) -> Value {
    date_of(chrono::NaiveDate::default().and_time(t))
}

/// Node: src/admin-company/admin-company.controller.ts:66 (`GET /admin-company/:uuid`)
pub async fn h_find_one(
    State(state): State<AppState>,
    op: Op,
    audit: AuditHandle,
    Path(raw): Path<String>,
) -> ApiResult<Json<Value>> {
    gate(op, &audit, None, None)?;
    // UUID salah → Prisma melempar → `catch` → "Company not found".
    let Some(uuid) = db_uuid(&raw) else {
        return Ok(Json(fail("Company not found")));
    };
    match find_one(&state, &uuid).await {
        Ok(Some(data)) => Ok(Json(json!({
            "success": true,
            "message": "Success get data company",
            "data": data,
        }))),
        Ok(None) => Ok(Json(fail(format!("Data company by uuid {raw} not found")))),
        Err(err) => {
            tracing::warn!(error = %err, "findOne admin-company gagal");
            Ok(Json(fail("Company not found")))
        }
    }
}

async fn find_one(state: &AppState, uuid: &uuid::Uuid) -> sqlx::Result<Option<Value>> {
    let row: Option<DetailRow> = sqlx::query_as(&format!(
        r#"SELECT c.id, c.uuid, c.name, c.telp, c."timeZone", c."dailyReportSentAt", c.status,
                  c."isAgencyMode", c."isTrial", c."isAlllowDeleteScreenshot", c."isSupportOfflineMode",
                  c."useActivityTrack", c."fixedHourNotification", {BOOSTS_SQL} AS boosts
           FROM company c WHERE c.uuid = $1 ORDER BY c.id LIMIT 1"#
    ))
    .bind(uuid)
    .fetch_optional(&state.db)
    .await?;
    let Some(c) = row else { return Ok(None) };

    type MemberRow = (
        i32,
        i32,
        Option<i32>,
        Option<uuid::Uuid>,
        Option<String>,
        Option<String>,
        Option<i32>,
    );
    let members: Vec<MemberRow> = sqlx::query_as(
        r#"SELECT uc.id, uc.status, u.id, u.uuid, u.name, u.username, u.status
           FROM "userCompany" uc LEFT JOIN users u ON u.id = uc."userId"
           WHERE uc."companyId" = $1 ORDER BY u.username ASC"#,
    )
    .bind(c.id)
    .fetch_all(&state.db)
    .await?;
    let user_company: Vec<Value> = members
        .into_iter()
        .map(|(id, status, uid, uuuid, name, username, ustatus)| {
            let user = uid.map_or(Value::Null, |uid| {
                json!({
                    "id": uid,
                    "uuid": uuuid.map(|u| u.to_string()),
                    "name": name,
                    "username": username,
                    "status": ustatus,
                })
            });
            json!({ "id": id, "status": status, "user": user })
        })
        .collect();

    type SubRow = (
        i32,
        uuid::Uuid,
        NaiveDateTime,
        NaiveDateTime,
        String,
        Option<uuid::Uuid>,
        Option<String>,
        Option<f64>,
    );
    let subs: Vec<SubRow> = sqlx::query_as(
        r#"SELECT s.id, s.uuid, s."startDate", s."endDate", s.status::text, pt.uuid, pt.name, pt."basePrice"
           FROM subscriptions s LEFT JOIN "pricingTiers" pt ON pt.id = s."pricingTierId"
           WHERE s."companyId" = $1 ORDER BY s.id"#,
    )
    .bind(c.id)
    .fetch_all(&state.db)
    .await?;
    let subscriptions: Vec<Value> = subs
        .into_iter()
        .map(|(id, u, start, end, status, tu, tn, tb)| {
            let tier = tu.map_or(Value::Null, |tu| {
                json!({ "uuid": tu.to_string(), "name": tn, "basePrice": tb.map_or(Value::Null, num) })
            });
            json!({
                "id": id,
                "uuid": u.to_string(),
                "startDate": date_of(start),
                "endDate": date_of(end),
                "status": status,
                "pricingTier": tier,
            })
        })
        .collect();

    let trials: Vec<(NaiveDateTime, NaiveDateTime)> = sqlx::query_as(
        r#"SELECT "startDate", "endDate" FROM trials WHERE "companyId" = $1 ORDER BY id"#,
    )
    .bind(c.id)
    .fetch_all(&state.db)
    .await?;
    let trial: Vec<Value> = trials
        .into_iter()
        .map(|(s, e)| json!({ "startDate": date_of(s), "endDate": date_of(e) }))
        .collect();

    Ok(Some(json!({
        "id": c.id,
        "uuid": c.uuid.to_string(),
        "name": c.name,
        "telp": c.telp,
        "timeZone": c.time_zone,
        "dailyReportSentAt": time_of_day(c.daily_report_sent_at),
        "status": c.status,
        "isAgencyMode": c.is_agency_mode,
        "isTrial": c.is_trial,
        "isAlllowDeleteScreenshot": c.is_allow_delete_screenshot,
        "isSupportOfflineMode": c.is_support_offline_mode,
        "useActivityTrack": c.use_activity_track,
        "fixedHourNotification": c.fixed_hour_notification,
        "userCompany": user_company,
        "subscriptions": subscriptions,
        "trial": trial,
        "planBoost": to_plan_boost_status(&c.boosts.0),
    })))
}

// ── GET /admin-company/user/{uuid} ──────────────────────────────────────────

/// Node: src/admin-company/admin-company.controller.ts:79 (`GET /admin-company/user/:uuid`)
pub async fn h_find_user(
    State(state): State<AppState>,
    op: Op,
    audit: AuditHandle,
    Path(raw): Path<String>,
) -> ApiResult<Json<Value>> {
    gate(op, &audit, None, None)?;
    let failed = || Json(fail("User not found"));
    let Some(uuid) = db_uuid(&raw) else {
        return Ok(failed());
    };
    let user: Option<(i32, uuid::Uuid, String, String, i32)> = match sqlx::query_as(
        "SELECT id, uuid, name, username, status FROM users WHERE uuid = $1 ORDER BY id LIMIT 1",
    )
    .bind(uuid)
    .fetch_optional(&state.db)
    .await
    {
        Ok(u) => u,
        Err(_) => return Ok(failed()),
    };
    let data = match user {
        None => Value::Null,
        Some((id, u, name, username, status)) => {
            let companies: Vec<(Option<String>,)> = match sqlx::query_as(
                r#"SELECT c.name FROM "userCompany" uc LEFT JOIN company c ON c.id = uc."companyId"
                   WHERE uc."userId" = $1 ORDER BY uc.id"#,
            )
            .bind(id)
            .fetch_all(&state.db)
            .await
            {
                Ok(c) => c,
                Err(_) => return Ok(failed()),
            };
            let user_company: Vec<Value> = companies
                .into_iter()
                .map(|(n,)| json!({ "company": n.map_or(Value::Null, |n| json!({ "name": n })) }))
                .collect();
            json!({
                "id": id,
                "uuid": u.to_string(),
                "name": name,
                "username": username,
                "status": status,
                "userCompany": user_company,
            })
        }
    };
    Ok(Json(json!({
        "success": true,
        "message": "Success get data user by uuid",
        "data": data,
    })))
}

// ── PATCH /admin-company/set/subscriptions/{uuid} ───────────────────────────

const SUBSCRIPTION_STATUSES: [&str; 5] =
    ["ACTIVE", "INACTIVE", "CANCELLED", "PENDING", "SUSPENDED"];

fn validate_set_subscription(body: &Value) -> ApiResult<Map<String, Value>> {
    let m = as_obj(body);
    let mut v = V::new();
    let end = m.get("endAt");
    v.not_empty("endAt", end);
    v.is_iso("endAt", end);
    v.is_in("status", m.get("status"), &SUBSCRIPTION_STATUSES);
    if !crate::val::absent(m.get("maxUsers")) {
        v.is_int("maxUsers", m.get("maxUsers"));
    }
    if !crate::val::absent(m.get("pricingUuid")) {
        v.is_uuid("pricingUuid", m.get("pricingUuid"));
    }
    v.finish()?;
    Ok(m)
}

/// Body audit = `req.body` mentah.
async fn audit_patch(
    state: &AppState,
    op: &Operator,
    ctx: &AuditCtx,
    action: &'static str,
    raw: &str,
    body: &Value,
    response: &Value,
) {
    if response.get("success") == Some(&Value::Bool(false)) {
        return;
    }
    audit_after_success(
        state,
        op,
        ctx,
        AuditMeta {
            action,
            target_type: "company",
            company_from: Some(CompanyFrom::Param),
        },
        Some(raw),
        Some(body),
    )
    .await;
}

/// Node: src/admin-company/admin-company.controller.ts:87 (`PATCH /admin-company/set/subscriptions/:uuid`)
pub async fn h_set_subscription(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw): Path<String>,
    body: RawBody,
) -> ApiResult<Json<Value>> {
    let op = gate(op, &audit, Some(&body), None)?;
    let body = body_value(body);
    let dto = validate_set_subscription(&body)?;
    let res = match set_subscription(&state, &audit, &raw, &dto).await {
        Ok(v) => v,
        Err(
            e @ ApiError::Body {
                status: StatusCode::CONFLICT,
                ..
            },
        ) => return Err(e),
        Err(e) => fail(non_empty(error_text(&e), "Cannot update subscription")),
    };
    audit_patch(
        &state,
        &op,
        &ctx,
        "company.subscription_set",
        &raw,
        &body,
        &res,
    )
    .await;
    Ok(Json(res))
}

fn non_empty(s: String, fallback: &str) -> String {
    if s.is_empty() { fallback.to_owned() } else { s }
}

/// `error.message`.
pub fn error_text(e: &ApiError) -> String {
    match e {
        ApiError::Message { message, .. } | ApiError::Plain { message, .. } => message.clone(),
        ApiError::Body { body, .. } => body
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        ApiError::UniqueViolation(m) | ApiError::BadJson(m) => m.clone(),
        ApiError::Validation(m) => m.join(","),
        ApiError::Bare(s) => s.canonical_reason().unwrap_or_default().to_owned(),
        ApiError::Internal(e) => e.to_string(),
    }
}

#[derive(sqlx::FromRow)]
struct LatestSub {
    id: i32,
    #[sqlx(rename = "pricingTierId")]
    pricing_tier_id: i32,
    #[sqlx(rename = "pendingPricingTierId")]
    pending_pricing_tier_id: Option<i32>,
    status: String,
    #[sqlx(rename = "paypalSubscriptionId")]
    paypal_subscription_id: Option<String>,
}

#[derive(sqlx::FromRow)]
struct NewTier {
    id: i32,
    uuid: uuid::Uuid,
    name: String,
    #[sqlx(rename = "maxUsers")]
    max_users: i32,
    #[sqlx(rename = "pricePerUserUsd")]
    price_per_user_usd: f64,
    #[sqlx(rename = "basePrice")]
    base_price: f64,
    #[sqlx(rename = "billingInterval")]
    billing_interval: String,
    #[sqlx(rename = "maxProject")]
    max_project: i32,
    #[sqlx(rename = "screenshotResolution")]
    screenshot_resolution: String,
    #[sqlx(rename = "screenshotRetentionDays")]
    screenshot_retention_days: i32,
    #[sqlx(rename = "reportExportsPerMonth")]
    report_exports_per_month: i32,
    #[sqlx(rename = "dataHistoryMonths")]
    data_history_months: i32,
    #[sqlx(rename = "budgetAlertDeliveryChannel")]
    budget_alert_delivery_channel: String,
    #[sqlx(rename = "clientProofExportAllowed")]
    client_proof_export_allowed: bool,
}

async fn set_subscription(
    state: &AppState,
    audit: &AuditHandle,
    raw: &str,
    dto: &Map<String, Value>,
) -> ApiResult<Value> {
    let uuid = match mz_core::prisma::parse_uuid(raw) {
        Ok(u) => u,
        Err(msg) => return Ok(fail(msg)),
    };
    let company: Option<(i32, bool, i32)> = sqlx::query_as(
        r#"SELECT id, "isTrial", "maxUsers" FROM company WHERE uuid = $1 ORDER BY id LIMIT 1"#,
    )
    .bind(uuid)
    .fetch_optional(&state.db)
    .await?;
    let Some((company_id, is_trial, company_max_users)) = company else {
        return Ok(fail("Company not found"));
    };

    // Langganan manual hanya lewat endpoint operator (spec 007 FR-023).
    if current_sub(&state.db, company_id)
        .await?
        .is_some_and(|s| s.is_manual())
    {
        return Err(coded(
            StatusCode::CONFLICT,
            "MANUAL_BILLING_USE_NEW_ENDPOINT",
            "This company is on manual billing; use the manual subscription endpoints (activate / extend / stop) instead.",
        ));
    }

    let sub: Option<LatestSub> = sqlx::query_as(
        r#"SELECT id, "pricingTierId", "pendingPricingTierId", status::text AS status, "paypalSubscriptionId"
           FROM subscriptions WHERE "companyId" = $1 ORDER BY id DESC LIMIT 1"#,
    )
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(sub) = sub else {
        return Ok(fail("Subscription not found for this company"));
    };

    let mut new_tier: Option<NewTier> = None;
    if let Some(p) = str_of(dto, "pricingUuid").filter(|s| !s.is_empty()) {
        new_tier = sqlx::query_as(
            r#"SELECT id, uuid, name, "maxUsers", "pricePerUserUsd", "basePrice", "billingInterval",
                      "maxProject", "screenshotResolution", "screenshotRetentionDays", "reportExportsPerMonth",
                      "dataHistoryMonths", "budgetAlertDeliveryChannel"::text AS "budgetAlertDeliveryChannel",
                      "clientProofExportAllowed"
               FROM "pricingTiers" WHERE uuid = $1::uuid ORDER BY id LIMIT 1"#,
        )
        .bind(p)
        .fetch_optional(&state.db)
        .await?;
        if new_tier.is_none() {
            return Ok(fail("Pricing tier not found"));
        }
    }
    let tier_changed = new_tier
        .as_ref()
        .is_some_and(|t| t.id != sub.pricing_tier_id);
    if tier_changed && sub.pending_pricing_tier_id.is_some() {
        return Ok(fail(
            "Subscription has a pending plan change; resolve it before changing the pricing tier",
        ));
    }

    let max_users = match dto.get("maxUsers").filter(|v| !v.is_null()) {
        Some(v) => match v.as_f64().and_then(mz_core::jsfmt::as_i32) {
            Some(n) => n,
            None => return Ok(fail("Invalid value for argument `maxUsers`")),
        },
        None => match (&new_tier, tier_changed) {
            (Some(t), true) => t.max_users,
            _ => company_max_users,
        },
    };
    let status = str_of(dto, "status").unwrap_or_default().to_owned();
    let end_at_raw = str_of(dto, "endAt").unwrap_or_default();
    let Some(end_date) = parse_js_date(end_at_raw) else {
        return Ok(fail(
            "Invalid value for argument `endDate`: Provided Date object is invalid. Expected Date.",
        ));
    };
    let now = now_naive();

    let mut start_date: Option<NaiveDateTime> = None;
    let mut auto_renew: Option<bool> = None;
    // Some(None) = tulis NULL.
    let mut next_invoice: Option<Option<NaiveDateTime>> = None;
    let mut paypal_status: Option<String> = None;
    if status == "ACTIVE" {
        if sub.status != "ACTIVE" {
            // `!subscription.startDate` tidak mungkin (kolom wajib).
            start_date = Some(now);
        }
        if sub.paypal_subscription_id.is_some() {
            auto_renew = Some(true);
            next_invoice = Some(Some(end_date - chrono::Duration::days(3)));
        }
    }
    if matches!(status.as_str(), "INACTIVE" | "CANCELLED" | "SUSPENDED") {
        auto_renew = Some(false);
        next_invoice = Some(None);
    }

    // Sinkron status PayPal (sebelum tulisan DB, sama dengan Node).
    if let Some(pid) = sub.paypal_subscription_id.as_deref() {
        let paypal = PaypalClient::from_state(state);
        let remote = paypal
            .get_subscription(pid)
            .await
            .map_err(|e| ApiError::internal(anyhow::anyhow!("{}", e.message)))?;
        let remote_status = remote.get("status").cloned().unwrap_or(Value::Null);
        let rs = remote_status.as_str();
        let pp = |e: mz_paypal::PaypalError| ApiError::internal(anyhow::anyhow!("{}", e.message));
        if status == "SUSPENDED" && rs == Some("ACTIVE") {
            paypal
                .suspend_subscription(pid, Some("Suspended by admin"))
                .await
                .map_err(pp)?;
            paypal_status = Some("SUSPENDED".into());
        } else if status == "ACTIVE" && rs == Some("SUSPENDED") {
            paypal
                .activate_subscription(pid, Some("Activated by admin"))
                .await
                .map_err(pp)?;
            paypal_status = Some("ACTIVE".into());
        } else if status == "CANCELLED" && rs != Some("CANCELLED") {
            paypal
                .cancel_subscription(pid, Some("Cancelled by admin"))
                .await
                .map_err(pp)?;
            paypal_status = Some("CANCELLED".into());
        } else {
            // `paypalStatus = paypalSub.status` (undefined = kolom tidak disentuh).
            match remote_status {
                Value::String(s) => paypal_status = Some(s),
                Value::Null => {}
                other => paypal_status = Some(other.to_string()),
            }
        }
    }

    // Tulis subscription + company dalam satu transaksi.
    let mut qb =
        sqlx::QueryBuilder::<sqlx::Postgres>::new(r#"UPDATE subscriptions SET "endDate" = "#);
    qb.push_bind(end_date)
        .push(", status = ")
        .push_bind(status.clone())
        .push(r#"::"SubscriptionStatus", "maxUsers" = "#)
        .push_bind(max_users);
    if let (true, Some(t)) = (tier_changed, &new_tier) {
        qb.push(r#", "pricingTierId" = "#)
            .push_bind(t.id)
            .push(r#", "pricePerUser" = "#)
            .push_bind(t.price_per_user_usd)
            .push(r#", "billingInterval" = "#)
            .push_bind(t.billing_interval.clone());
    }
    if let Some(s) = start_date {
        qb.push(r#", "startDate" = "#).push_bind(s);
    }
    if let Some(a) = auto_renew {
        qb.push(r#", "autoRenew" = "#).push_bind(a);
    }
    if let Some(n) = next_invoice {
        qb.push(r#", "nextInvoiceDate" = "#).push_bind(n);
    }
    if let Some(p) = &paypal_status {
        qb.push(r#", "paypalStatus" = "#).push_bind(p.clone());
    }
    qb.push(" WHERE id = ").push_bind(sub.id);

    let mut company_data: Vec<(String, Bind)> = vec![("maxUsers".into(), Bind::I32(max_users))];
    if status == "ACTIVE" && is_trial {
        company_data.push(("isTrial".into(), Bind::Bool(false)));
    }
    if let (true, Some(t)) = (tier_changed, &new_tier) {
        let mut limits = obj(json!({
            "maxProject": t.max_project,
            "screenshotResolution": t.screenshot_resolution,
            "screenshotRetentionDays": t.screenshot_retention_days,
            "reportExportsPerMonth": t.report_exports_per_month,
            "reportExportsUsedThisMonth": 0,
            "reportExportsResetAt": Value::Null,
            "dataHistoryMonths": t.data_history_months,
            "budgetAlertDeliveryChannel": t.budget_alert_delivery_channel,
            "clientProofExportAllowed": t.client_proof_export_allowed,
        }));
        let tier_obj =
            obj(json!({ "basePrice": t.base_price, "pricePerUserUsd": t.price_per_user_usd }));
        if is_paid_tier(Some(&tier_obj)) {
            for (k, v) in paid_tier_company_limits() {
                limits.insert(k, v);
            }
        }
        for (k, b) in binds_of(&limits) {
            company_data.push((k.clone(), b));
            if k == "reportExportsUsedThisMonth" {
                company_data.push(("reportExportsResetAt".into(), Bind::Ts(now)));
            }
        }
    }

    let mut tx = state.db.begin().await?;
    qb.build().execute(&mut *tx).await?;
    update_company(&mut tx, audit, company_id, &company_data).await?;
    tx.commit().await?;

    if tier_changed {
        reapply_plan_override_after_tier_sync(state, audit, company_id).await;
        clear_user_me_cache(state, CompanyWhere::Id(company_id)).await;
    }

    Ok(json!({
        "success": true,
        "message": "Subscription updated successfully",
        "data": {
            "pricingChanged": tier_changed,
            "pricingTier": new_tier.map_or(Value::Null, |t| json!({ "uuid": t.uuid.to_string(), "name": t.name })),
        },
    }))
}

// ── PATCH offline-mode / activity-track ─────────────────────────────────────

/// `company.updateMany({ where: { uuid }, data: { <col>: value } })` + audit.
async fn update_flag(
    state: &AppState,
    audit: &AuditHandle,
    uuid: &uuid::Uuid,
    column: &str,
    value: bool,
) -> sqlx::Result<u64> {
    let u = *uuid;
    let pre = audit
        .pre_read_many(
            Model::Company,
            Lookup::filter(move |qb| {
                qb.push("uuid = ").push_bind(u);
            }),
        )
        .await;
    let n = sqlx::query(&format!(
        r#"UPDATE company SET "{column}" = $2 WHERE uuid = $1"#
    ))
    .bind(uuid)
    .bind(value)
    .execute(&state.db)
    .await?
    .rows_affected();
    audit
        .record_update_many(
            Model::Company,
            pre,
            n,
            &Row::new().with(column, value),
            json!({ "uuid": uuid.to_string() }),
        )
        .await;
    Ok(n)
}

fn validate_bool(body: &Value, key: &str) -> ApiResult<bool> {
    let m = as_obj(body);
    let mut v = V::new();
    v.is_boolean(key, m.get(key));
    v.finish()?;
    Ok(m.get(key).and_then(Value::as_bool).unwrap_or(false))
}

/// Node: src/admin-company/admin-company.controller.ts:111 (`PATCH /admin-company/set/offline-mode/:uuid`)
pub async fn h_offline_mode(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw): Path<String>,
    body: RawBody,
) -> ApiResult<Json<Value>> {
    let op = gate(op, &audit, Some(&body), None)?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let body = body_value(body);
    let value = validate_bool(&body, "isSupportOfflineMode")?;
    let uuid = uuid_pipe(&raw)?;
    let res = match update_flag(&state, &audit, &uuid, "isSupportOfflineMode", value).await {
        Ok(0) => fail("Company not found"),
        Ok(_) => json!({
            "success": true,
            "message": "Offline mode support updated successfully",
            "data": { "uuid": raw, "isSupportOfflineMode": value },
        }),
        Err(e) => fail(non_empty(
            e.to_string(),
            "Cannot update offline mode support",
        )),
    };
    audit_patch(&state, &op, &ctx, "company.offline_mode", &raw, &body, &res).await;
    Ok(Json(res))
}

/// Node: src/admin-company/admin-company.controller.ts:129 (`PATCH /admin-company/set/activity-track/:uuid`)
pub async fn h_activity_track(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw): Path<String>,
    body: RawBody,
) -> ApiResult<Json<Value>> {
    let op = gate(op, &audit, Some(&body), None)?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let body = body_value(body);
    let value = validate_bool(&body, "useActivityTrack")?;
    let uuid = uuid_pipe(&raw)?;
    let res = match update_flag(&state, &audit, &uuid, "useActivityTrack", value).await {
        Ok(0) => fail("Company not found"),
        Ok(_) => {
            clear_user_me_cache(&state, CompanyWhere::Uuid(&uuid)).await;
            json!({
                "success": true,
                "message": "Activity tracking updated successfully",
                "data": { "uuid": raw, "useActivityTrack": value },
            })
        }
        Err(e) => fail(non_empty(e.to_string(), "Cannot update activity tracking")),
    };
    audit_patch(
        &state,
        &op,
        &ctx,
        "company.activity_track",
        &raw,
        &body,
        &res,
    )
    .await;
    Ok(Json(res))
}

// ── PATCH plan-boost ────────────────────────────────────────────────────────

/// Node: src/admin-company/admin-company.controller.ts:147 (`PATCH /admin-company/set/plan-boost/:uuid`)
pub async fn h_plan_boost(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw): Path<String>,
    body: RawBody,
) -> ApiResult<Json<Value>> {
    let op = gate(op, &audit, Some(&body), None)?;
    // Node: galat body (pipe global) muncul sebelum `ParseUUIDPipe` param (diverifikasi paritas).
    let body = body_value(body);
    let m = as_obj(&body);
    let mut v = V::new();
    v.is_in("product", m.get("product"), &["SALES", "TIME"]);
    v.is_boolean("enabled", m.get("enabled"));
    v.finish()?;
    let uuid = uuid_pipe(&raw)?;
    let product = str_of(&m, "product").unwrap_or_default().to_owned();
    let enabled = m.get("enabled").and_then(Value::as_bool).unwrap_or(false);

    let res = match set_boost(&state, &audit, &uuid, &product, enabled, Some(op.id)).await {
        Ok(None) => fail("Company not found"),
        Ok(Some(data)) => {
            let changed = data.get("changed") == Some(&Value::Bool(true));
            let word = if enabled { "enabled" } else { "disabled" };
            let message = if changed {
                format!("Plan boost {product} {word} successfully")
            } else {
                format!("Plan boost {product} already {word}")
            };
            json!({ "success": true, "message": message, "data": data })
        }
        Err(e) => fail(non_empty(error_text(&e), "Cannot update plan boost")),
    };
    audit_patch(&state, &op, &ctx, "company.plan_boost", &raw, &body, &res).await;
    Ok(Json(res))
}

// ── PATCH set/trial ─────────────────────────────────────────────────────────

/// Prisma menerima string `DateTime` hanya RFC 3339 lengkap (dengan zona).
fn prisma_datetime(raw: &str) -> Option<NaiveDateTime> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|d| d.with_timezone(&chrono::Utc).naive_utc())
}

/// Node: src/admin-company/admin-company.controller.ts:175 (`PATCH /admin-company/set/trial/:uuid`)
pub async fn h_set_trial(
    State(state): State<AppState>,
    op: Op,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(raw): Path<String>,
    body: RawBody,
) -> ApiResult<Json<Value>> {
    let op = gate(op, &audit, Some(&body), None)?;
    let body = body_value(body);
    let m = as_obj(&body);
    let mut v = V::new();
    v.is_boolean("isTrial", m.get("isTrial"));
    v.not_empty("endAt", m.get("endAt"));
    v.is_iso("endAt", m.get("endAt"));
    v.finish()?;
    let is_trial = m.get("isTrial").and_then(Value::as_bool).unwrap_or(false);
    let end_raw = str_of(&m, "endAt").unwrap_or_default().to_owned();

    let res = match set_trial(&state, &audit, &raw, is_trial, &end_raw).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "setTrial gagal");
            fail("Cannot update company trial")
        }
    };
    audit_patch(&state, &op, &ctx, "company.trial", &raw, &body, &res).await;
    Ok(Json(res))
}

async fn set_trial(
    state: &AppState,
    audit: &AuditHandle,
    raw: &str,
    is_trial: bool,
    end_raw: &str,
) -> ApiResult<Value> {
    let Some(uuid) = db_uuid(raw) else {
        return Ok(fail("Cannot update company trial"));
    };
    let company: Option<i32> =
        sqlx::query_scalar("SELECT id FROM company WHERE uuid = $1 ORDER BY id LIMIT 1")
            .bind(uuid)
            .fetch_optional(&state.db)
            .await?;
    let Some(company_id) = company else {
        return Ok(fail("Company not found"));
    };
    let Some(end) = prisma_datetime(end_raw) else {
        return Ok(fail("Cannot update company trial"));
    };
    let mut tx = state.db.begin().await?;
    update_company(
        &mut tx,
        audit,
        company_id,
        &[("isTrial".into(), Bind::Bool(is_trial))],
    )
    .await?;
    sqlx::query(r#"UPDATE trials SET "endDate" = $2 WHERE "companyId" = $1"#)
        .bind(company_id)
        .bind(end)
        .execute(&mut *tx)
        .await?;
    sqlx::query(r#"UPDATE subscriptions SET "endDate" = $2 WHERE "companyId" = $1"#)
        .bind(company_id)
        .bind(end)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(json!({ "success": true, "message": "Trial updated successfully", "data": Value::Null }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_query_validation_messages() {
        let err = parse_list_query(Some("pageNumber=abc&orderBy=a&orderBy=b")).err();
        match err {
            Some(ApiError::Validation(m)) => assert_eq!(
                m,
                vec![
                    "pageNumber must be a number conforming to the specified constraints",
                    "orderBy must be a string"
                ]
            ),
            other => panic!("{other:?}"),
        }
        let q = parse_list_query(Some("pageNumber=2x&itemsPerPage=10&search=a")).ok();
        assert_eq!(q.as_ref().and_then(|q| q.page_number), Some(2));
        assert_eq!(q.as_ref().and_then(|q| q.items_per_page), Some(10));
    }

    #[test]
    fn set_subscription_dto_messages() {
        match validate_set_subscription(&json!({ "maxUsers": 1.5, "pricingUuid": "x" })) {
            Err(ApiError::Validation(m)) => assert_eq!(
                m,
                vec![
                    "endAt should not be empty",
                    "endAt must be a valid ISO 8601 date string",
                    "status must be one of the following values: ACTIVE, INACTIVE, CANCELLED, PENDING, SUSPENDED",
                    "maxUsers must be an integer number",
                    "pricingUuid must be a UUID",
                ]
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn like_pattern_keeps_wildcards_like_prisma() {
        assert_eq!(like_pattern("50%_a"), "%50%_a%");
    }

    #[test]
    fn prisma_datetime_needs_zone() {
        assert!(prisma_datetime("2026-12-31T00:00:00.000Z").is_some());
        assert!(prisma_datetime("2026-12-31").is_none());
    }

    #[test]
    fn time_column_is_epoch_date() {
        let t = NaiveTime::from_hms_opt(8, 30, 0).unwrap_or_default();
        assert_eq!(time_of_day(t), json!("1970-01-01T08:30:00.000Z"));
    }
}
