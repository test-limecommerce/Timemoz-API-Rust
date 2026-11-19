//! Port `src/paypal/paypal-admin.controller.ts` (15 route operator `/admin/paypal/**`).
//!
//! Guard Node: class-level `JwtAuthGuard` (strategi user; token operator `{username, sub}`
//! lolos tanpa cek pencabutan) → `RouteSpec::jwt()`, lalu `JwtAdminAuthGuard` per handler →
//! extractor [`Operator`]. Tanpa `@OperatorRoles` (semua role operator aktif boleh).
//! Semua handler membalas 200 `{ success: false, message: error.message }` saat gagal;
//! `OperatorAuditInterceptor` hanya menulis bila respons bukan `success: false`.

use axum::Json;
use axum::extract::{Path, State};
use mz_audit::{AuditHandle, Lookup, Model};
use mz_core::jsfmt::{num, to_fixed};
use mz_core::{ApiError, ApiResult, AppState};
use mz_operator::{AuditCtx, AuditMeta, Operator, audit_after_success};
use mz_paypal::{CreatePlanPayload, PaypalClient};
use serde_json::{Map, Value, json};

use crate::webhook::RawBody;
use mz_core::extract::ValidatedJson;

type Body = ValidatedJson<RawBody>;

// ── validasi (ValidationPipe global, tanpa whitelist) ──────────────────────

#[derive(Default)]
struct V(Vec<String>);

impl V {
    /// `@IsString()` (+ `@IsOptional()` bila `optional`).
    fn string(&mut self, body: &Value, field: &str, optional: bool) {
        let v = body.get(field);
        if optional && matches!(v, None | Some(Value::Null)) {
            return;
        }
        if !matches!(v, Some(Value::String(_))) {
            self.0.push(format!("{field} must be a string"));
        }
    }

    /// `@IsNumber()` di atas `@Min(min)` (+ `@IsOptional()`). class-validator menjalankan dekorator
    /// dari BAWAH ke atas, jadi pesan `@Min` keluar lebih dulu (paritas `billing mcp mutasi`).
    fn number_min(&mut self, body: &Value, field: &str, min: f64, optional: bool) {
        let v = body.get(field);
        if optional && matches!(v, None | Some(Value::Null)) {
            return;
        }
        let n = v.and_then(Value::as_f64);
        if n.is_none_or(|n| n < min) {
            self.0.push(format!(
                "{field} must not be less than {}",
                mz_core::jsfmt::num_string(min)
            ));
        }
        if n.is_none() {
            self.0.push(format!(
                "{field} must be a number conforming to the specified constraints"
            ));
        }
    }

    fn finish(self) -> ApiResult<()> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(ApiError::Validation(self.0))
        }
    }
}

fn s<'a>(body: &'a Value, field: &str) -> Option<&'a str> {
    body.get(field).and_then(Value::as_str)
}

fn f(body: &Value, field: &str) -> f64 {
    body.get(field).and_then(Value::as_f64).unwrap_or(0.0)
}

/// `dto.x` bila truthy (string non-kosong).
fn truthy<'a>(body: &'a Value, field: &str) -> Option<&'a str> {
    s(body, field).filter(|v| !v.is_empty())
}

fn fail(message: impl Into<String>) -> Json<Value> {
    Json(json!({ "success": false, "message": message.into() }))
}

/// `{ ...plan }`: objek disalin, string jadi `{0:'a',...}`, selain itu kosong.
fn spread(v: &Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m.clone(),
        Value::String(t) => t
            .chars()
            .enumerate()
            .map(|(i, c)| (i.to_string(), Value::String(c.to_string())))
            .collect(),
        _ => Map::new(),
    }
}

/// Galat yang di Node berasal dari Prisma/axios di dalam blok `try`.
fn db_message(err: &sqlx::Error) -> String {
    err.to_string()
}

async fn audited(
    state: &AppState,
    op: &Operator,
    ctx: &AuditCtx,
    action: &'static str,
    target_type: &'static str,
    body: &Value,
    resp: Json<Value>,
) -> Json<Value> {
    if resp.0.get("success") != Some(&Value::Bool(false)) {
        audit_after_success(
            state,
            op,
            ctx,
            AuditMeta {
                action,
                target_type,
                company_from: None,
            },
            None,
            Some(body),
        )
        .await;
    }
    resp
}

fn pp(state: &AppState) -> PaypalClient {
    PaypalClient::from_state(state)
}

// ── produk ──────────────────────────────────────────────────────────────────

/// Node: src/paypal/paypal-admin.controller.ts:42 (`POST /admin/paypal/products`, 200)
pub async fn create_product(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    ValidatedJson(RawBody(body)): Body,
) -> ApiResult<Json<Value>> {
    let mut v = V::default();
    v.string(&body, "name", false);
    v.string(&body, "description", false);
    v.finish()?;
    let resp = match pp(&state)
        .create_product(
            s(&body, "name").unwrap_or_default(),
            s(&body, "description").unwrap_or_default(),
            None,
        )
        .await
    {
        Ok(product) => Json(json!({
            "success": true,
            "message": "Product created successfully",
            "data": product,
        })),
        Err(e) => fail(e.message),
    };
    Ok(audited(
        &state,
        &op,
        &ctx,
        "paypal.plan.product_create",
        "system",
        &body,
        resp,
    )
    .await)
}

/// Node: src/paypal/paypal-admin.controller.ts:66 (`GET /admin/paypal/products/:productId`)
pub async fn get_product(
    State(state): State<AppState>,
    _op: Operator,
    Path(product_id): Path<String>,
) -> Json<Value> {
    match pp(&state).get_product(&product_id).await {
        Ok(product) => Json(json!({ "success": true, "data": product })),
        Err(e) => fail(e.message),
    }
}

// ── plan ────────────────────────────────────────────────────────────────────

/// `pricingTiers.updateMany({ where: { uuid }, data })` — galat (mis. UUID tidak sah) = pesan.
async fn update_tiers_by_uuid(
    state: &AppState,
    uuid: &str,
    set: &str,
    bind: impl FnOnce(
        sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments>,
    ) -> sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments>,
) -> Result<u64, String> {
    let sql = format!(r#"UPDATE "pricingTiers" SET {set} WHERE uuid = $1::uuid"#);
    let q = bind(sqlx::query(&sql).bind(uuid));
    q.execute(&state.db)
        .await
        .map(|r| r.rows_affected())
        .map_err(|e| db_message(&e))
}

/// Node: src/paypal/paypal-admin.controller.ts:84 (`POST /admin/paypal/plans`, 200)
pub async fn create_plan(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    ValidatedJson(RawBody(body)): Body,
) -> ApiResult<Json<Value>> {
    let mut v = V::default();
    v.string(&body, "productId", false);
    v.string(&body, "name", false);
    v.string(&body, "description", false);
    v.number_min(&body, "priceUsd", 0.0, false);
    v.string(&body, "pricingTierUuid", true);
    v.finish()?;
    let payload = CreatePlanPayload {
        product_id: s(&body, "productId").unwrap_or_default().to_owned(),
        name: s(&body, "name").unwrap_or_default().to_owned(),
        description: s(&body, "description").unwrap_or_default().to_owned(),
        price_usd: f(&body, "priceUsd"),
        interval_unit: None,
        interval_count: None,
    };
    let resp = match pp(&state).create_plan(&payload).await {
        Err(e) => fail(e.message),
        Ok(plan) => {
            let linked = match (truthy(&body, "pricingTierUuid"), plan.get("id")) {
                // `data: { paypalPlanId: undefined }` = tanpa perubahan.
                (Some(uuid), Some(id)) => {
                    let id = id
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| id.to_string());
                    update_tiers_by_uuid(&state, uuid, r#""paypalPlanId" = $2"#, |q| q.bind(id))
                        .await
                        .map(drop)
                }
                _ => Ok(()),
            };
            match linked {
                Ok(()) => Json(json!({
                    "success": true,
                    "message": "Plan created successfully",
                    "data": plan,
                })),
                Err(m) => fail(m),
            }
        }
    };
    Ok(audited(
        &state,
        &op,
        &ctx,
        "paypal.plan.create",
        "system",
        &body,
        resp,
    )
    .await)
}

/// Node: src/paypal/paypal-admin.controller.ts:119 (`POST /admin/paypal/plans/quantity-based`, 200)
pub async fn create_quantity_based_plan(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    ValidatedJson(RawBody(body)): Body,
) -> ApiResult<Json<Value>> {
    let mut v = V::default();
    v.string(&body, "productId", false);
    v.string(&body, "name", false);
    v.string(&body, "description", false);
    v.number_min(&body, "pricePerUserUsd", 0.0, false);
    v.string(&body, "billingInterval", true);
    v.number_min(&body, "minUsers", 1.0, true);
    v.string(&body, "pricingTierUuid", true);
    v.finish()?;

    let interval = truthy(&body, "billingInterval")
        .unwrap_or("MONTH")
        .to_owned();
    let min_users = body
        .get("minUsers")
        .and_then(Value::as_f64)
        .filter(|n| *n != 0.0)
        .unwrap_or(1.0);
    let price_per_user = f(&body, "pricePerUserUsd");
    let multiplier = if interval == "YEAR" { 12.0 } else { 1.0 };
    let payload = CreatePlanPayload {
        product_id: s(&body, "productId").unwrap_or_default().to_owned(),
        name: s(&body, "name").unwrap_or_default().to_owned(),
        description: s(&body, "description").unwrap_or_default().to_owned(),
        price_usd: price_per_user * multiplier,
        interval_unit: Some(interval.clone()),
        interval_count: Some(1),
    };
    let resp = match pp(&state).create_quantity_based_plan(&payload).await {
        Err(e) => fail(e.message),
        Ok(plan) => {
            let linked = match truthy(&body, "pricingTierUuid") {
                Some(uuid) => {
                    let plan_id = plan.get("id").map(|id| {
                        id.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| id.to_string())
                    });
                    let product_id = s(&body, "productId").unwrap_or_default().to_owned();
                    let interval = interval.clone();
                    match mz_core::jsfmt::as_i32(min_users).filter(|_| min_users.fract() == 0.0) {
                        None => Err(format!(
                            "Invalid value for argument `minUsers`: expected Int, provided {}",
                            mz_core::jsfmt::num_string(min_users)
                        )),
                        Some(min_users) => update_tiers_by_uuid(
                            &state,
                            uuid,
                            r#""paypalPlanId" = COALESCE($2, "paypalPlanId"), "paypalProductId" = $3,
                               "pricePerUserUsd" = $4, "billingInterval" = $5, "minUsers" = $6"#,
                            |q| {
                                q.bind(plan_id)
                                    .bind(product_id)
                                    .bind(price_per_user)
                                    .bind(interval)
                                    .bind(min_users)
                            },
                        )
                        .await
                        .map(drop),
                    }
                }
                None => Ok(()),
            };
            match linked {
                Err(m) => fail(m),
                Ok(()) => {
                    let mut data = spread(&plan);
                    data.insert("pricePerUserUsd".into(), num(price_per_user));
                    data.insert("billingInterval".into(), Value::String(interval));
                    data.insert("minUsers".into(), num(min_users));
                    data.insert("quantitySupported".into(), Value::Bool(true));
                    Json(json!({
                        "success": true,
                        "message": "Quantity-based plan created successfully",
                        "data": data,
                    }))
                }
            }
        }
    };
    Ok(audited(
        &state,
        &op,
        &ctx,
        "paypal.plan.create_quantity_based",
        "system",
        &body,
        resp,
    )
    .await)
}

/// Node: src/paypal/paypal-admin.controller.ts:175 (`GET /admin/paypal/plans`)
pub async fn list_plans(State(state): State<AppState>, _op: Operator) -> Json<Value> {
    match pp(&state).list_plans(None).await {
        Ok(plans) => Json(json!({ "success": true, "data": plans })),
        Err(e) => fail(e.message),
    }
}

/// Node: src/paypal/paypal-admin.controller.ts:193 (`GET /admin/paypal/plans/:planId`)
pub async fn get_plan(
    State(state): State<AppState>,
    op: Operator,
    Path(plan_id): Path<String>,
) -> Json<Value> {
    get_plan_by_id(&state, &op, &plan_id).await
}

async fn get_plan_by_id(state: &AppState, _op: &Operator, plan_id: &str) -> Json<Value> {
    match pp(state).get_plan(plan_id).await {
        Ok(plan) => Json(json!({ "success": true, "data": plan })),
        Err(e) => fail(e.message),
    }
}

/// `GET /admin/paypal/plans/link` & `/plans/quantity-based`: di Nest jatuh ke
/// `GET plans/:planId` (path statis hanya punya POST); axum butuh route eksplisit.
pub async fn get_plan_link(State(state): State<AppState>, op: Operator) -> Json<Value> {
    get_plan_by_id(&state, &op, "link").await
}

/// Lihat [`get_plan_link`].
pub async fn get_plan_quantity_based(State(state): State<AppState>, op: Operator) -> Json<Value> {
    get_plan_by_id(&state, &op, "quantity-based").await
}

/// Node: src/paypal/paypal-admin.controller.ts:211 (`POST /admin/paypal/plans/:planId/update-name`, 200)
pub async fn update_plan_name(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    Path(plan_id): Path<String>,
    ValidatedJson(RawBody(body)): Body,
) -> ApiResult<Json<Value>> {
    let mut v = V::default();
    v.string(&body, "name", false);
    v.string(&body, "description", true);
    v.finish()?;
    let name = s(&body, "name").unwrap_or_default();
    let resp = match pp(&state)
        .update_plan_name(&plan_id, name, s(&body, "description"))
        .await
    {
        Ok(()) => {
            let mut data = Map::new();
            data.insert("planId".into(), Value::String(plan_id.clone()));
            data.insert("name".into(), Value::String(name.to_owned()));
            // `description: undefined` hilang dari JSON.
            if let Some(d) = body.get("description") {
                data.insert("description".into(), d.clone());
            }
            Json(json!({
                "success": true,
                "message": "Plan name updated successfully",
                "data": data,
            }))
        }
        Err(e) => fail(e.message),
    };
    Ok(audited(
        &state,
        &op,
        &ctx,
        "paypal.plan.update_name",
        "system",
        &body,
        resp,
    )
    .await)
}

/// Node: src/paypal/paypal-admin.controller.ts:237 (`POST /admin/paypal/plans/:planId/update-price`, 200)
pub async fn update_plan_price(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    Path(plan_id): Path<String>,
    ValidatedJson(RawBody(body)): Body,
) -> ApiResult<Json<Value>> {
    let mut v = V::default();
    v.number_min(&body, "pricePerUserUsd", 0.0, false);
    v.string(&body, "pricingTierUuid", true);
    v.finish()?;
    let resp = update_plan_price_inner(&state, &plan_id, &body)
        .await
        .unwrap_or_else(fail);
    Ok(audited(
        &state,
        &op,
        &ctx,
        "paypal.plan.update_price",
        "system",
        &body,
        resp,
    )
    .await)
}

async fn update_plan_price_inner(
    state: &AppState,
    plan_id: &str,
    body: &Value,
) -> Result<Json<Value>, String> {
    let price_per_user = f(body, "pricePerUserUsd");
    let tier_uuid = truthy(body, "pricingTierUuid");
    let mut interval = "MONTH".to_owned();
    if let Some(uuid) = tier_uuid {
        let tier: Option<String> = sqlx::query_scalar(
            r#"SELECT "billingInterval" FROM "pricingTiers" WHERE uuid = $1::uuid ORDER BY id LIMIT 1"#,
        )
        .bind(uuid)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| db_message(&e))?;
        if let Some(i) = tier.filter(|i| !i.is_empty()) {
            interval = i;
        }
    }
    let multiplier = if interval == "YEAR" { 12.0 } else { 1.0 };
    let per_cycle = price_per_user * multiplier;
    pp(state)
        .update_plan_price(plan_id, per_cycle)
        .await
        .map_err(|e| e.message)?;
    if let Some(uuid) = tier_uuid {
        update_tiers_by_uuid(state, uuid, r#""pricePerUserUsd" = $2"#, |q| {
            q.bind(price_per_user)
        })
        .await?;
    }
    Ok(Json(json!({
        "success": true,
        "message": format!(
            "Plan price updated to ${} per user (billed ${} per {}). Existing subscribers will be charged the new rate on their next billing cycle.",
            to_fixed(price_per_user, 2),
            to_fixed(per_cycle, 2),
            interval.to_lowercase()
        ),
        "data": {
            "planId": plan_id,
            "pricePerUserUsd": num(price_per_user),
            "billingInterval": interval,
            "pricePerCycle": num(per_cycle),
        },
    })))
}

/// Node: src/paypal/paypal-admin.controller.ts:292 (`POST /admin/paypal/plans/:planId/deactivate`, 200)
pub async fn deactivate_plan(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    Path(plan_id): Path<String>,
    ValidatedJson(RawBody(body)): Body,
) -> Json<Value> {
    let resp = match pp(&state).deactivate_plan(&plan_id).await {
        Ok(()) => Json(json!({ "success": true, "message": "Plan deactivated successfully" })),
        Err(e) => fail(e.message),
    };
    audited(
        &state,
        &op,
        &ctx,
        "paypal.plan.deactivate",
        "system",
        &body,
        resp,
    )
    .await
}

/// Node: src/paypal/paypal-admin.controller.ts:312 (`POST /admin/paypal/plans/link`, 200)
pub async fn link_plan(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    ValidatedJson(RawBody(body)): Body,
) -> ApiResult<Json<Value>> {
    let mut v = V::default();
    v.string(&body, "paypalPlanId", false);
    v.string(&body, "pricingTierUuid", false);
    v.finish()?;
    let plan_id = s(&body, "paypalPlanId").unwrap_or_default().to_owned();
    let resp = match update_tiers_by_uuid(
        &state,
        s(&body, "pricingTierUuid").unwrap_or_default(),
        r#""paypalPlanId" = $2"#,
        |q| q.bind(plan_id),
    )
    .await
    {
        Ok(0) => fail("Pricing tier not found"),
        Ok(_) => {
            Json(json!({ "success": true, "message": "Plan linked to pricing tier successfully" }))
        }
        Err(m) => fail(m),
    };
    Ok(audited(
        &state,
        &op,
        &ctx,
        "paypal.plan.link",
        "pricing",
        &body,
        resp,
    )
    .await)
}

// ── subscription ────────────────────────────────────────────────────────────

/// Node: src/paypal/paypal-admin.controller.ts:343 (`GET /admin/paypal/subscriptions/:subscriptionId`)
pub async fn get_subscription(
    State(state): State<AppState>,
    _op: Operator,
    Path(subscription_id): Path<String>,
) -> Json<Value> {
    match pp(&state).get_subscription(&subscription_id).await {
        Ok(sub) => Json(json!({ "success": true, "data": sub })),
        Err(e) => fail(e.message),
    }
}

#[derive(Clone, Copy)]
enum SubAction {
    Cancel,
    Suspend,
    Activate,
}

impl SubAction {
    fn default_reason(self) -> &'static str {
        match self {
            Self::Cancel => "Cancelled by admin",
            Self::Suspend => "Suspended by admin",
            Self::Activate => "Activated by admin",
        }
    }
    fn status(self) -> &'static str {
        match self {
            Self::Cancel => "CANCELLED",
            Self::Suspend => "SUSPENDED",
            Self::Activate => "ACTIVE",
        }
    }
    fn action(self) -> &'static str {
        match self {
            Self::Cancel => "paypal.subscription.cancel",
            Self::Suspend => "paypal.subscription.suspend",
            Self::Activate => "paypal.subscription.activate",
        }
    }
    fn done(self) -> &'static str {
        match self {
            Self::Cancel => "Subscription cancelled successfully",
            Self::Suspend => "Subscription suspended successfully",
            Self::Activate => "Subscription activated successfully",
        }
    }
}

async fn change_subscription(
    state: &AppState,
    op: &Operator,
    ctx: &AuditCtx,
    subscription_id: &str,
    body: &Value,
    action: SubAction,
) -> Json<Value> {
    // `reason || default` (nilai non-string truthy tidak didukung klien PayPal → default).
    let reason = truthy(body, "reason").unwrap_or(action.default_reason());
    let client = pp(state);
    let called = match action {
        SubAction::Cancel => {
            client
                .cancel_subscription(subscription_id, Some(reason))
                .await
        }
        SubAction::Suspend => {
            client
                .suspend_subscription(subscription_id, Some(reason))
                .await
        }
        SubAction::Activate => {
            client
                .activate_subscription(subscription_id, Some(reason))
                .await
        }
    };
    let resp = match called {
        Err(e) => fail(e.message),
        Ok(()) => match sqlx::query(
            r#"UPDATE subscriptions SET status = $2::"SubscriptionStatus", "paypalStatus" = $2
                WHERE "paypalSubscriptionId" = $1"#,
        )
        .bind(subscription_id)
        .bind(action.status())
        .execute(&state.db)
        .await
        {
            Ok(_) => Json(json!({ "success": true, "message": action.done() })),
            Err(e) => fail(db_message(&e)),
        },
    };
    audited(state, op, ctx, action.action(), "subscription", body, resp).await
}

/// Node: src/paypal/paypal-admin.controller.ts:362 (`POST /admin/paypal/subscriptions/:subscriptionId/cancel`, 200)
pub async fn cancel_subscription(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    Path(id): Path<String>,
    ValidatedJson(RawBody(body)): Body,
) -> Json<Value> {
    change_subscription(&state, &op, &ctx, &id, &body, SubAction::Cancel).await
}

/// Node: src/paypal/paypal-admin.controller.ts:398 (`POST /admin/paypal/subscriptions/:subscriptionId/suspend`, 200)
pub async fn suspend_subscription(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    Path(id): Path<String>,
    ValidatedJson(RawBody(body)): Body,
) -> Json<Value> {
    change_subscription(&state, &op, &ctx, &id, &body, SubAction::Suspend).await
}

/// Node: src/paypal/paypal-admin.controller.ts:434 (`POST /admin/paypal/subscriptions/:subscriptionId/activate`, 200)
pub async fn activate_subscription(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    Path(id): Path<String>,
    ValidatedJson(RawBody(body)): Body,
) -> Json<Value> {
    change_subscription(&state, &op, &ctx, &id, &body, SubAction::Activate).await
}

/// Node: src/paypal/paypal-admin.controller.ts:470 (`POST /admin/paypal/subscriptions/:subscriptionId/update-quantity`, 200)
pub async fn update_subscription_quantity(
    State(state): State<AppState>,
    op: Operator,
    ctx: AuditCtx,
    audit: AuditHandle,
    Path(id): Path<String>,
    ValidatedJson(RawBody(body)): Body,
) -> Json<Value> {
    let resp = update_quantity_inner(&state, &audit, &id, &body)
        .await
        .unwrap_or_else(fail);
    audited(
        &state,
        &op,
        &ctx,
        "paypal.subscription.update_quantity",
        "subscription",
        &body,
        resp,
    )
    .await
}

async fn update_quantity_inner(
    state: &AppState,
    audit: &AuditHandle,
    subscription_id: &str,
    body: &Value,
) -> Result<Json<Value>, String> {
    // `@Body('quantity') quantity: number` tanpa validasi/transformasi.
    let raw = body.get("quantity");
    let quantity = match raw {
        None => return Err("Cannot read properties of undefined (reading 'toString')".into()),
        Some(Value::Null) => {
            return Err("Cannot read properties of null (reading 'toString')".into());
        }
        Some(v) => v
            .as_f64()
            .filter(|n| n.fract() == 0.0)
            .and_then(mz_core::jsfmt::as_i32)
            .ok_or_else(|| "quantity must be an integer".to_owned())?,
    };
    pp(state)
        .update_subscription_quantity(subscription_id, i64::from(quantity))
        .await
        .map_err(|e| e.message)?;
    let sub: Option<(i32, i32)> = sqlx::query_as(
        r#"SELECT id, "companyId" FROM subscriptions WHERE "paypalSubscriptionId" = $1 ORDER BY id LIMIT 1"#,
    )
    .bind(subscription_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| db_message(&e))?;
    if let Some((sid, company_id)) = sub {
        sqlx::query("UPDATE subscriptions SET quantity = $2 WHERE id = $1")
            .bind(sid)
            .bind(quantity)
            .execute(&state.db)
            .await
            .map_err(|e| db_message(&e))?;
        company_max_users(state, audit, company_id, quantity)
            .await
            .map_err(|e| db_message(&e))?;
    }
    Ok(Json(json!({
        "success": true,
        "message": format!("Subscription quantity updated to {quantity}"),
        "data": { "quantity": quantity },
    })))
}

/// `company.update({ data: { maxUsers } })` lewat audit (pelaku: operator dari extractor).
async fn company_max_users(
    state: &AppState,
    audit: &AuditHandle,
    company_id: i32,
    max_users: i32,
) -> sqlx::Result<()> {
    let pre = audit.pre_read(Model::Company, Lookup::Id(company_id)).await;
    let n = sqlx::query(r#"UPDATE company SET "maxUsers" = $2 WHERE id = $1"#)
        .bind(company_id)
        .bind(max_users)
        .execute(&state.db)
        .await?
        .rows_affected();
    if n == 0 {
        // Prisma `update` tanpa baris = galat P2025.
        return Err(sqlx::Error::RowNotFound);
    }
    if let Some(row) = audit
        .result_row(&state.db, Model::Company, Lookup::Id(company_id))
        .await
    {
        audit.record_update(Model::Company, pre, &row).await;
    }
    Ok(())
}
