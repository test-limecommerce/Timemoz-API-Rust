//! Uji integrasi 15 route operator `/admin/paypal/**` (PayPal TIRUAN).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::type_complexity
)]

mod common;

use common::*;
use mz_testkit::TestApp;
use serde_json::{Value, json};

/// `[action, targetType, targetUuid, companyId, after, reason]` operator.
async fn op_audit(app: &TestApp, op: i32) -> Vec<Value> {
    let rows: Vec<(
        String,
        String,
        Option<String>,
        Option<i32>,
        Option<Value>,
        Option<String>,
    )> = sqlx::query_as(
        r#"SELECT action, "targetType", "targetUuid"::text, "companyId", after, reason
                 FROM "operatorAuditLog" WHERE "operatorId" = $1 ORDER BY id"#,
    )
    .bind(op)
    .fetch_all(app.db())
    .await
    .unwrap();
    rows.into_iter()
        .map(|(a, t, u, c, af, r)| json!([a, t, u, c, af, r]))
        .collect()
}

/// Tier uji baru (tidak mengubah tier fixture) → (id, uuid).
async fn tier(app: &TestApp, interval: &str) -> (i32, String) {
    let (id, uuid): (i32, String) = sqlx::query_as(&format!(
        r#"INSERT INTO "pricingTiers" (name, status, "billingInterval", "pricePerUserUsd")
           VALUES ('IT PP {}', 1, '{interval}', 5) RETURNING id, uuid::text"#,
        uniq()
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    (id, uuid)
}

#[tokio::test]
async fn auth_rules() {
    let Some((app, mock)) = app().await else {
        return;
    };
    mock.on("GET", "/v1/billing/plans*", 200, json!({ "plans": [] }));
    // Tanpa token / token user biasa → 401.
    let r = app.get("/admin/paypal/plans").send().await;
    r.assert_status(401);
    assert_eq!(
        r.json,
        json!({ "message": "Unauthorized", "statusCode": 401 })
    );
    let fx = fixture(&app, STARTER_MONTHLY, false, "").await;
    app.get("/admin/paypal/plans")
        .token(&app.token(fx.owner))
        .send()
        .await
        .assert_status(401);
    // Operator nonaktif → 401.
    let (op, token) = operator(&app, "SUPPORT").await;
    app.exec(&format!("UPDATE operators SET status = 0 WHERE id = {op}"))
        .await;
    app.get("/admin/paypal/plans")
        .token(&token)
        .send()
        .await
        .assert_status(401);
    // Semua role operator aktif boleh (tanpa @OperatorRoles).
    for role in ["OWNER", "FINANCE", "SUPPORT"] {
        let (_, token) = operator(&app, role).await;
        let r = app.get("/admin/paypal/plans").token(&token).send().await;
        r.assert_ok();
        assert_eq!(r.json, json!({ "success": true, "data": { "plans": [] } }));
    }
    let list = mock.requests_to("GET", "/v1/billing/plans");
    assert_eq!(list[0].query, "page_size=20&page=1&total_required=true");
    // POST tanpa token ditolak sebelum validasi body.
    app.post("/admin/paypal/products")
        .json(&json!({}))
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
async fn products_and_plans() {
    let Some((app, mock)) = app().await else {
        return;
    };
    let (op, token) = operator(&app, "FINANCE").await;

    // Produk: validasi, sukses, gagal PayPal (tanpa audit).
    let r = app
        .post("/admin/paypal/products")
        .token(&token)
        .json(&json!({ "name": 5 }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!(["name must be a string", "description must be a string"])
    );
    mock.on(
        "POST",
        "/v1/catalogs/products",
        201,
        json!({ "id": "PROD-1", "name": "T" }),
    );
    let body = json!({ "name": "T", "description": "D", "password": "x" });
    let r = app
        .post("/admin/paypal/products")
        .token(&token)
        .json(&body)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.json,
        json!({ "success": true, "message": "Product created successfully", "data": { "id": "PROD-1", "name": "T" } })
    );
    assert_eq!(
        mock.requests_to("POST", "/v1/catalogs/products")[0].json(),
        json!({ "name": "T", "description": "D", "type": "SERVICE", "category": "SOFTWARE" })
    );
    mock.on(
        "POST",
        "/v1/catalogs/products",
        500,
        json!({ "name": "INTERNAL" }),
    );
    let r = app
        .post("/admin/paypal/products")
        .token(&token)
        .json(&body)
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({ "success": false, "message": "Request failed with status code 500" })
    );
    mock.on(
        "GET",
        "/v1/catalogs/products/PROD-1",
        200,
        json!({ "id": "PROD-1" }),
    );
    let r = app
        .get("/admin/paypal/products/PROD-1")
        .token(&token)
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({ "success": true, "data": { "id": "PROD-1" } })
    );
    mock.on(
        "GET",
        "/v1/catalogs/products/NOPE",
        404,
        json!({ "name": "RESOURCE_NOT_FOUND" }),
    );
    let r = app
        .get("/admin/paypal/products/NOPE")
        .token(&token)
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({ "success": false, "message": "Request failed with status code 404" })
    );

    // Plan + tautan tier.
    let (tier_id, tier_uuid) = tier(&app, "MONTH").await;
    let r = app
        .post("/admin/paypal/plans")
        .token(&token)
        .json(&json!({ "productId": "PROD-1", "name": "P", "description": "D", "priceUsd": -1 }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!(["priceUsd must not be less than 0"])
    );
    let r = app
        .post("/admin/paypal/plans")
        .token(&token)
        .json(&json!({ "productId": "PROD-1", "name": "P", "description": "D" }))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!([
            "priceUsd must not be less than 0",
            "priceUsd must be a number conforming to the specified constraints"
        ])
    );
    mock.on(
        "POST",
        "/v1/billing/plans",
        201,
        json!({ "id": "P-NEW1", "status": "ACTIVE" }),
    );
    let r = app
        .post("/admin/paypal/plans")
        .token(&token)
        .json(&json!({ "productId": "PROD-1", "name": "P", "description": "D", "priceUsd": 10.0, "pricingTierUuid": tier_uuid }))
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.json["data"]["id"], "P-NEW1");
    let plan: Option<String> = app
        .scalar(&format!(
            r#"SELECT "paypalPlanId" FROM "pricingTiers" WHERE id = {tier_id}"#
        ))
        .await;
    assert_eq!(plan.as_deref(), Some("P-NEW1"));
    let sent = mock.requests_to("POST", "/v1/billing/plans")[0].json();
    assert_eq!(
        sent["billing_cycles"][0]["frequency"]["interval_unit"],
        "MONTH"
    );

    // Plan quantity-based tahunan: harga × 12, tier diperbarui, data = {...plan, ...}.
    let (yt_id, yt_uuid) = tier(&app, "MONTH").await;
    let r = app
        .post("/admin/paypal/plans/quantity-based")
        .token(&token)
        .json(
            &json!({ "productId": "PROD-1", "name": "Q", "description": "D", "pricePerUserUsd": 2.5,
                       "billingInterval": "YEAR", "minUsers": 0, "pricingTierUuid": yt_uuid }),
        )
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!(["minUsers must not be less than 1"])
    );
    let r = app
        .post("/admin/paypal/plans/quantity-based")
        .token(&token)
        .json(
            &json!({ "productId": "PROD-1", "name": "Q", "description": "D", "pricePerUserUsd": 2.5,
                       "billingInterval": "YEAR", "minUsers": 3, "pricingTierUuid": yt_uuid }),
        )
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(
        r.json["data"],
        json!({ "id": "P-NEW1", "status": "ACTIVE", "pricePerUserUsd": 2.5, "billingInterval": "YEAR",
                "minUsers": 3, "quantitySupported": true })
    );
    let sent = mock.requests_to("POST", "/v1/billing/plans")[1].json();
    assert_eq!(
        sent["billing_cycles"][0]["frequency"]["interval_unit"],
        "YEAR"
    );
    assert_eq!(
        sent["billing_cycles"][0]["pricing_scheme"]["fixed_price"]["value"],
        "30.00"
    );
    let row: Value = app
        .scalar(&format!(
            r#"SELECT json_build_object('plan', "paypalPlanId", 'product', "paypalProductId", 'price', "pricePerUserUsd",
                   'interval', "billingInterval", 'min', "minUsers") FROM "pricingTiers" WHERE id = {yt_id}"#
        ))
        .await;
    assert_eq!(
        row,
        json!({ "plan": "P-NEW1", "product": "PROD-1", "price": 2.5, "interval": "YEAR", "min": 3 })
    );

    // Tier UUID tidak sah → success:false (galat DB), plan tetap dibuat di PayPal (seperti Node).
    let r = app
        .post("/admin/paypal/plans")
        .token(&token)
        .json(&json!({ "productId": "PROD-1", "name": "P", "description": "D", "priceUsd": 1, "pricingTierUuid": "nope" }))
        .send()
        .await;
    assert_eq!(r.json["success"], false);

    // GET plan / statis yang jatuh ke :planId.
    mock.on("GET", "/v1/billing/plans/*", 200, json!({ "id": "X" }));
    for path in ["P-NEW1", "link", "quantity-based"] {
        let r = app
            .get(&format!("/admin/paypal/plans/{path}"))
            .token(&token)
            .send()
            .await;
        assert_eq!(
            r.json,
            json!({ "success": true, "data": { "id": "X" } }),
            "{path}"
        );
        assert_eq!(
            mock.requests_to("GET", &format!("/v1/billing/plans/{path}"))
                .len(),
            1
        );
    }

    // Ganti nama (description opsional → hilang dari data).
    mock.on("PATCH", "/v1/billing/plans/*", 204, Value::Null);
    let r = app
        .post("/admin/paypal/plans/P-NEW1/update-name")
        .token(&token)
        .json(&json!({ "name": "Baru" }))
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({ "success": true, "message": "Plan name updated successfully", "data": { "planId": "P-NEW1", "name": "Baru" } })
    );
    assert_eq!(
        mock.requests_to("PATCH", "/v1/billing/plans/P-NEW1")[0].json(),
        json!([{ "op": "replace", "path": "/name", "value": "Baru" }])
    );

    // Ganti harga dengan tier tahunan.
    let (pt_id, pt_uuid) = tier(&app, "YEAR").await;
    mock.on("POST", "/v1/billing/plans/*", 204, Value::Null);
    let r = app
        .post("/admin/paypal/plans/P-NEW1/update-price")
        .token(&token)
        .json(&json!({ "pricePerUserUsd": 4.5, "pricingTierUuid": pt_uuid }))
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(
        r.json,
        json!({
            "success": true,
            "message": "Plan price updated to $4.50 per user (billed $54.00 per year). Existing subscribers will be charged the new rate on their next billing cycle.",
            "data": { "planId": "P-NEW1", "pricePerUserUsd": 4.5, "billingInterval": "YEAR", "pricePerCycle": 54 }
        })
    );
    let price: f64 = app
        .scalar(&format!(
            r#"SELECT "pricePerUserUsd" FROM "pricingTiers" WHERE id = {pt_id}"#
        ))
        .await;
    assert!((price - 4.5).abs() < 1e-9);

    // Nonaktifkan + tautkan.
    let r = app
        .post("/admin/paypal/plans/P-NEW1/deactivate")
        .token(&token)
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({ "success": true, "message": "Plan deactivated successfully" })
    );
    let r = app
        .post("/admin/paypal/plans/link")
        .token(&token)
        .json(&json!({ "paypalPlanId": "P-LINK", "pricingTierUuid": "3f2c1a9e-8b7d-4c6e-9a1b-2d3e4f5a6b7c" }))
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({ "success": false, "message": "Pricing tier not found" })
    );
    let r = app
        .post("/admin/paypal/plans/link")
        .token(&token)
        .json(&json!({ "paypalPlanId": "P-LINK", "pricingTierUuid": pt_uuid }))
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({ "success": true, "message": "Plan linked to pricing tier successfully" })
    );
    let r = app
        .post("/admin/paypal/plans/link")
        .token(&token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);

    // Jejak operator: hanya yang sukses, body tanpa rahasia.
    let audit = op_audit(&app, op).await;
    let actions: Vec<&str> = audit.iter().map(|a| a[0].as_str().unwrap()).collect();
    assert_eq!(
        actions,
        [
            "paypal.plan.product_create",
            "paypal.plan.create",
            "paypal.plan.create_quantity_based",
            "paypal.plan.update_name",
            "paypal.plan.update_price",
            "paypal.plan.deactivate",
            "paypal.plan.link",
        ]
    );
    assert_eq!(
        audit[0],
        json!(["paypal.plan.product_create", "system", null, null, { "name": "T", "description": "D" }, null])
    );
    assert_eq!(audit[5][4], json!({}));
    assert_eq!(audit[6][1], "pricing");
}

#[tokio::test]
async fn subscriptions() {
    let Some((app, mock)) = app().await else {
        return;
    };
    let (op, token) = operator(&app, "SUPPORT").await;
    let fx = fixture(&app, STARTER_MONTHLY, false, "").await;
    mock.on(
        "GET",
        &format!("/v1/billing/subscriptions/{}", fx.pp),
        200,
        json!({ "id": fx.pp, "status": "ACTIVE" }),
    );
    let r = app
        .get(&format!("/admin/paypal/subscriptions/{}", fx.pp))
        .token(&token)
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({ "success": true, "data": { "id": fx.pp, "status": "ACTIVE" } })
    );

    mock.on("POST", "/v1/billing/subscriptions/*", 204, Value::Null);
    let base = format!("/admin/paypal/subscriptions/{}", fx.pp);
    let r = app
        .post(&format!("{base}/suspend"))
        .token(&token)
        .json(&json!({ "reason": "audit" }))
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({ "success": true, "message": "Subscription suspended successfully" })
    );
    assert_eq!(
        mock.requests_to(
            "POST",
            &format!("/v1/billing/subscriptions/{}/suspend", fx.pp)
        )[0]
        .json(),
        json!({ "reason": "audit" })
    );
    let s = sub_row(&app, fx.sub).await;
    assert_eq!(
        (&s["status"], &s["paypalStatus"]),
        (&json!("SUSPENDED"), &json!("SUSPENDED"))
    );

    let r = app
        .post(&format!("{base}/activate"))
        .token(&token)
        .send()
        .await;
    assert_eq!(r.json["message"], "Subscription activated successfully");
    assert_eq!(
        mock.requests_to(
            "POST",
            &format!("/v1/billing/subscriptions/{}/activate", fx.pp)
        )[0]
        .json(),
        json!({ "reason": "Activated by admin" })
    );
    assert_eq!(sub_row(&app, fx.sub).await["status"], "ACTIVE");

    // update-quantity: PATCH PayPal, quantity & company.maxUsers.
    mock.on("PATCH", "/v1/billing/subscriptions/*", 204, Value::Null);
    let r = app
        .post(&format!("{base}/update-quantity"))
        .token(&token)
        .json(&json!({ "quantity": 7 }))
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(
        r.json,
        json!({ "success": true, "message": "Subscription quantity updated to 7", "data": { "quantity": 7 } })
    );
    assert_eq!(
        mock.requests_to("PATCH", &format!("/v1/billing/subscriptions/{}", fx.pp))[0].json(),
        json!([{ "op": "replace", "path": "/quantity", "value": "7" }])
    );
    assert_eq!(sub_row(&app, fx.sub).await["quantity"], 7);
    assert_eq!(company_row(&app, fx.company).await["maxUsers"], 7);
    // Audit company atas nama operator.
    assert!(
        wait_until(
            &app,
            &format!(
                r#"SELECT EXISTS (SELECT 1 FROM "auditLog" WHERE "entityType" = 'company' AND "entityId" = {}
                     AND "actorName" LIKE 'Timemoz Support (%')"#,
                fx.company
            )
        )
        .await
    );
    let r = app
        .post(&format!("{base}/update-quantity"))
        .token(&token)
        .json(&json!({}))
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({ "success": false, "message": "Cannot read properties of undefined (reading 'toString')" })
    );

    // Cancel: PayPal gagal → success:false, DB tidak berubah, tanpa audit.
    mock.on(
        "POST",
        &format!("/v1/billing/subscriptions/{}/cancel", fx.pp),
        422,
        json!({ "details": [{ "issue": "SOMETHING" }] }),
    );
    let r = app
        .post(&format!("{base}/cancel"))
        .token(&token)
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({ "success": false, "message": "Request failed with status code 422" })
    );
    assert_eq!(sub_row(&app, fx.sub).await["status"], "ACTIVE");
    // SUBSCRIPTION_STATUS_INVALID dianggap sudah batal.
    mock.on(
        "POST",
        &format!("/v1/billing/subscriptions/{}/cancel", fx.pp),
        422,
        json!({ "details": [{ "issue": "SUBSCRIPTION_STATUS_INVALID" }] }),
    );
    let r = app
        .post(&format!("{base}/cancel"))
        .token(&token)
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({ "success": true, "message": "Subscription cancelled successfully" })
    );
    let s = sub_row(&app, fx.sub).await;
    assert_eq!(
        (&s["status"], &s["paypalStatus"]),
        (&json!("CANCELLED"), &json!("CANCELLED"))
    );

    let audit = op_audit(&app, op).await;
    let actions: Vec<&str> = audit.iter().map(|a| a[0].as_str().unwrap()).collect();
    assert_eq!(
        actions,
        [
            "paypal.subscription.suspend",
            "paypal.subscription.activate",
            "paypal.subscription.update_quantity",
            "paypal.subscription.cancel",
        ]
    );
    assert_eq!(
        audit[0],
        json!(["paypal.subscription.suspend", "subscription", null, null, { "reason": "audit" }, "audit"])
    );
}
