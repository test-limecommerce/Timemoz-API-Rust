//! Uji klien PayPal terhadap server tiruan (tanpa DB, tanpa PayPal sungguhan).
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::time::Duration;

use mz_int_common::mock::MockServer;
use mz_paypal::{
    CreatePlanPayload, CreateSubscriptionPayload, PaypalClient, PaypalConfig, get_approval_url,
};
use serde_json::{Value, json};

fn client(mock: &MockServer) -> PaypalClient {
    PaypalClient::new(
        PaypalConfig {
            base_url: mock.url(),
            client_id: Some("cid".into()),
            client_secret: Some("sec".into()),
            brand_name: "Timemoz".into(),
            timeout: Duration::from_millis(500),
        },
        reqwest::Client::new(),
    )
}

async fn setup() -> (MockServer, PaypalClient) {
    let mock = MockServer::start().await;
    mock.on(
        "POST",
        "/v1/oauth2/token",
        200,
        json!({ "access_token": "tok1", "token_type": "Bearer", "expires_in": 3600 }),
    );
    let c = client(&mock);
    (mock, c)
}

#[tokio::test]
async fn token_is_cached_and_sent_as_basic_then_bearer() {
    let (mock, c) = setup().await;
    mock.on(
        "GET",
        "/v1/catalogs/products/P1",
        200,
        json!({ "id": "P1" }),
    );
    assert_eq!(c.get_product("P1").await.unwrap()["id"], "P1");
    assert_eq!(c.get_product("P1").await.unwrap()["id"], "P1");
    let tok = mock.requests_to("POST", "/v1/oauth2/token");
    assert_eq!(tok.len(), 1, "token harus di-cache");
    // base64("cid:sec") = Y2lkOnNlYw==
    assert_eq!(tok[0].header("authorization"), Some("Basic Y2lkOnNlYw=="));
    assert_eq!(
        tok[0].form(),
        vec![("grant_type".to_owned(), "client_credentials".to_owned())]
    );
    let r = mock.requests_to("GET", "/v1/catalogs/products/P1");
    assert_eq!(r[0].header("authorization"), Some("Bearer tok1"));
}

#[tokio::test]
async fn short_lived_token_is_refetched() {
    let mock = MockServer::start().await;
    // expires_in 30 dtk < margin 60 dtk -> langsung dianggap kedaluwarsa.
    mock.on(
        "POST",
        "/v1/oauth2/token",
        200,
        json!({ "access_token": "t", "expires_in": 30 }),
    );
    mock.on("GET", "/v1/billing/plans/X", 200, json!({}));
    let c = client(&mock);
    c.get_plan("X").await.unwrap();
    c.get_plan("X").await.unwrap();
    assert_eq!(mock.requests_to("POST", "/v1/oauth2/token").len(), 2);
}

#[tokio::test]
async fn token_failure_is_error_with_axios_message() {
    let mock = MockServer::start().await;
    mock.on(
        "POST",
        "/v1/oauth2/token",
        401,
        json!({ "error": "invalid_client" }),
    );
    let c = client(&mock);
    let e = c.get_plan("X").await.unwrap_err();
    assert_eq!(e.to_string(), "Request failed with status code 401");
    assert_eq!(e.status, Some(401));
    assert_eq!(e.data.unwrap()["error"], "invalid_client");
    assert!(mock.requests_to("GET", "/v1/billing/plans/X").is_empty());
}

#[tokio::test]
async fn api_error_carries_response_data() {
    let (mock, c) = setup().await;
    mock.on(
        "POST",
        "/v1/billing/plans",
        422,
        json!({ "name": "UNPROCESSABLE_ENTITY", "details": [{ "issue": "X" }] }),
    );
    let e = c
        .create_plan(&CreatePlanPayload {
            product_id: "P".into(),
            name: "n".into(),
            description: "d".into(),
            price_usd: 5.0,
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(e.to_string(), "Request failed with status code 422");
    assert_eq!(e.first_issue(), Some("X"));
}

#[tokio::test]
async fn timeout_and_refused_messages() {
    let (mock, c) = setup().await;
    // server tiruan tak punya delay; uji refused ke port tertutup.
    let _ = mock;
    let dead = PaypalClient::new(
        PaypalConfig {
            base_url: "http://127.0.0.1:1".into(),
            client_id: None,
            client_secret: None,
            brand_name: "Timemoz".into(),
            timeout: Duration::from_millis(500),
        },
        reqwest::Client::new(),
    );
    let e = dead.get_plan("X").await.unwrap_err();
    assert_eq!(e.to_string(), "connect ECONNREFUSED 127.0.0.1:1");
    assert!(e.data.is_none());
    drop(c);
}

#[tokio::test]
async fn create_plan_payloads() {
    let (mock, c) = setup().await;
    mock.on("POST", "/v1/billing/plans", 201, json!({ "id": "PLAN" }));
    let p = CreatePlanPayload {
        product_id: "PROD".into(),
        name: "Pro".into(),
        description: "Desc".into(),
        price_usd: 4.5,
        interval_unit: Some("YEAR".into()),
        interval_count: Some(2),
    };
    c.create_plan(&p).await.unwrap();
    c.create_quantity_based_plan(&CreatePlanPayload {
        interval_unit: None,
        interval_count: None,
        ..p
    })
    .await
    .unwrap();
    let r = mock.requests_to("POST", "/v1/billing/plans");
    let fixed = r[0].json();
    assert_eq!(
        fixed,
        json!({
            "product_id": "PROD", "name": "Pro", "description": "Desc", "status": "ACTIVE",
            "billing_cycles": [{
                "frequency": { "interval_unit": "YEAR", "interval_count": 2 },
                "tenure_type": "REGULAR", "sequence": 1, "total_cycles": 0,
                "pricing_scheme": { "fixed_price": { "value": "4.50", "currency_code": "USD" } }
            }],
            "payment_preferences": {
                "auto_bill_outstanding": true, "setup_fee_failure_action": "CONTINUE",
                "payment_failure_threshold": 3
            }
        })
    );
    assert!(fixed.get("quantity_supported").is_none());
    // urutan kunci seperti Node
    let keys: Vec<_> = r[1].json().as_object().unwrap().keys().cloned().collect();
    assert_eq!(
        keys,
        [
            "product_id",
            "name",
            "description",
            "status",
            "quantity_supported",
            "billing_cycles",
            "payment_preferences"
        ]
    );
    let q = r[1].json();
    assert_eq!(q["quantity_supported"], true);
    assert_eq!(
        q["billing_cycles"][0]["frequency"]["interval_unit"],
        "MONTH"
    );
    assert_eq!(q["billing_cycles"][0]["frequency"]["interval_count"], 1);
}

#[tokio::test]
async fn product_and_plan_admin_calls() {
    let (mock, c) = setup().await;
    mock.on("POST", "/v1/catalogs/products", 201, json!({ "id": "P" }));
    mock.on("GET", "/v1/billing/plans", 200, json!({ "plans": [] }));
    mock.on("POST", "/v1/billing/plans/PL/*", 204, Value::Null);
    mock.on("PATCH", "/v1/billing/plans/PL", 204, Value::Null);
    c.create_product("n", "d", None).await.unwrap();
    assert_eq!(
        mock.requests_to("POST", "/v1/catalogs/products")[0].json(),
        json!({ "name": "n", "description": "d", "type": "SERVICE", "category": "SOFTWARE" })
    );
    c.list_plans(None).await.unwrap();
    c.list_plans(Some("PROD")).await.unwrap();
    let l = mock.requests_to("GET", "/v1/billing/plans");
    assert_eq!(l[0].query, "page_size=20&page=1&total_required=true");
    assert_eq!(
        l[1].query,
        "product_id=PROD&page_size=20&page=1&total_required=true"
    );
    c.deactivate_plan("PL").await.unwrap();
    assert_eq!(
        mock.requests_to("POST", "/v1/billing/plans/PL/deactivate")[0].json(),
        json!({})
    );
    c.update_plan_name("PL", "N", Some("D")).await.unwrap();
    c.update_plan_name("PL", "N2", Some("")).await.unwrap();
    let p = mock.requests_to("PATCH", "/v1/billing/plans/PL");
    assert_eq!(
        p[0].json(),
        json!([{ "op": "replace", "path": "/name", "value": "N" },
               { "op": "replace", "path": "/description", "value": "D" }])
    );
    assert_eq!(p[1].json().as_array().unwrap().len(), 1);
    c.update_plan_price("PL", 7.0).await.unwrap();
    assert_eq!(
        mock.requests_to("POST", "/v1/billing/plans/PL/update-pricing-schemes")[0].json(),
        json!({ "pricing_schemes": [{ "billing_cycle_sequence": 1,
            "pricing_scheme": { "fixed_price": { "value": "7.00", "currency_code": "USD" } } }] })
    );
}

#[tokio::test]
async fn subscription_calls() {
    let (mock, c) = setup().await;
    mock.on(
        "POST",
        "/v1/billing/subscriptions",
        201,
        json!({ "id": "I-1", "links": [{ "rel": "self", "href": "s" }, { "rel": "approve", "href": "https://pp/approve" }] }),
    );
    mock.on("POST", "/v1/billing/subscriptions/I-1/*", 204, Value::Null);
    mock.on("PATCH", "/v1/billing/subscriptions/I-1", 204, Value::Null);
    mock.on(
        "GET",
        "/v1/billing/subscriptions/I-1",
        200,
        json!({ "status": "ACTIVE" }),
    );
    mock.on(
        "GET",
        "/v1/billing/subscriptions/I-1/transactions",
        200,
        json!({ "transactions": [] }),
    );

    let res = c
        .create_subscription(&CreateSubscriptionPayload {
            plan_id: "PL".into(),
            subscriber_email: "a@b.c".into(),
            subscriber_name: "Ana Maria Putri".into(),
            custom_id: Some("cust".into()),
            return_url: "https://r".into(),
            cancel_url: "https://c".into(),
            quantity: Some(3),
            start_time: Some("2026-12-01T00:00:00Z".into()),
        })
        .await
        .unwrap();
    assert_eq!(
        get_approval_url(&res).as_deref(),
        Some("https://pp/approve")
    );
    let b = mock.requests_to("POST", "/v1/billing/subscriptions")[0].json();
    assert_eq!(b["quantity"], "3");
    assert_eq!(
        b["subscriber"]["name"],
        json!({ "given_name": "Ana", "surname": "Maria Putri" })
    );
    assert_eq!(b["start_time"], "2026-12-01T00:00:00Z");
    assert_eq!(b["custom_id"], "cust");
    assert_eq!(b["application_context"]["user_action"], "SUBSCRIBE_NOW");
    assert_eq!(b["application_context"]["brand_name"], "Timemoz");

    // satu nama: surname = given
    c.create_subscription(&CreateSubscriptionPayload {
        plan_id: "PL".into(),
        subscriber_name: "Budi".into(),
        ..Default::default()
    })
    .await
    .unwrap();
    let b2 = mock.requests_to("POST", "/v1/billing/subscriptions")[1].json();
    assert_eq!(b2["quantity"], "1");
    assert_eq!(
        b2["subscriber"]["name"],
        json!({ "given_name": "Budi", "surname": "Budi" })
    );
    assert!(b2.get("start_time").is_none() && b2.get("custom_id").is_none());

    assert_eq!(c.get_subscription("I-1").await.unwrap()["status"], "ACTIVE");
    c.update_subscription_quantity("I-1", 5).await.unwrap();
    assert_eq!(
        mock.requests_to("PATCH", "/v1/billing/subscriptions/I-1")[0].json(),
        json!([{ "op": "replace", "path": "/quantity", "value": "5" }])
    );
    c.revise_subscription_quantity("I-1", 4, "r", "c")
        .await
        .unwrap();
    c.revise_subscription("I-1", "PL2", "r", "c").await.unwrap();
    let rev = mock.requests_to("POST", "/v1/billing/subscriptions/I-1/revise");
    assert_eq!(rev[0].json()["quantity"], "4");
    assert_eq!(rev[1].json()["plan_id"], "PL2");
    c.activate_subscription("I-1", None).await.unwrap();
    c.suspend_subscription("I-1", Some("why")).await.unwrap();
    assert_eq!(
        mock.requests_to("POST", "/v1/billing/subscriptions/I-1/activate")[0].json(),
        json!({ "reason": "Reactivating subscription" })
    );
    assert_eq!(
        mock.requests_to("POST", "/v1/billing/subscriptions/I-1/suspend")[0].json(),
        json!({ "reason": "why" })
    );
    c.list_subscription_transactions(
        "I-1",
        "2026-01-01T00:00:00.000Z",
        "2026-02-01T00:00:00.000Z",
    )
    .await
    .unwrap();
    assert_eq!(
        mock.requests_to("GET", "/v1/billing/subscriptions/I-1/transactions")[0].query,
        "start_time=2026-01-01T00:00:00.000Z&end_time=2026-02-01T00:00:00.000Z"
    );
    c.capture_subscription_payment("I-1", 12.345, "note")
        .await
        .unwrap();
    assert_eq!(
        mock.requests_to("POST", "/v1/billing/subscriptions/I-1/capture")[0].json(),
        json!({ "note": "note", "capture_type": "OUTSTANDING_BALANCE",
                "amount": { "currency_code": "USD", "value": "12.35" } })
    );
}

#[tokio::test]
async fn cancel_subscription_semantics() {
    let (mock, c) = setup().await;
    mock.on(
        "POST",
        "/v1/billing/subscriptions/OK/cancel",
        204,
        Value::Null,
    );
    mock.on(
        "POST",
        "/v1/billing/subscriptions/INV/cancel",
        422,
        json!({ "details": [{ "issue": "SUBSCRIPTION_STATUS_INVALID" }] }),
    );
    mock.on(
        "POST",
        "/v1/billing/subscriptions/BAD/cancel",
        500,
        json!({ "name": "INTERNAL" }),
    );
    c.cancel_subscription("OK", None).await.unwrap();
    assert_eq!(
        mock.requests_to("POST", "/v1/billing/subscriptions/OK/cancel")[0].json(),
        json!({ "reason": "Subscription cancelled by user" })
    );
    c.cancel_subscription("INV", None).await.unwrap(); // no-op
    let e = c.cancel_subscription("BAD", None).await.unwrap_err();
    assert_eq!(e.to_string(), "Request failed with status code 500");
}

#[tokio::test]
async fn orders_and_webhook_verification() {
    let (mock, c) = setup().await;
    mock.on("POST", "/v2/checkout/orders", 201, json!({ "id": "O1" }));
    mock.on(
        "POST",
        "/v2/checkout/orders/O1/capture",
        201,
        json!({ "status": "COMPLETED" }),
    );
    mock.on(
        "GET",
        "/v2/checkout/orders/O1",
        200,
        json!({ "status": "APPROVED" }),
    );
    c.create_order(3.0, "Proration", "r", "c").await.unwrap();
    let b = mock.requests_to("POST", "/v2/checkout/orders")[0].json();
    assert_eq!(b["intent"], "CAPTURE");
    assert_eq!(b["purchase_units"][0]["amount"]["value"], "3.00");
    assert_eq!(b["application_context"]["user_action"], "PAY_NOW");
    assert_eq!(c.capture_order("O1").await.unwrap()["status"], "COMPLETED");
    assert_eq!(
        mock.requests_to("POST", "/v2/checkout/orders/O1/capture")[0].json(),
        json!({})
    );
    assert_eq!(c.get_order("O1").await.unwrap()["status"], "APPROVED");

    let ev = json!({ "id": "WH-1" });
    let path = "/v1/notifications/verify-webhook-signature";
    mock.on(
        "POST",
        path,
        200,
        json!({ "verification_status": "SUCCESS" }),
    );
    assert!(
        c.verify_webhook_signature(
            "W",
            "tid",
            "tt",
            "https://cert",
            "SHA256withRSA",
            "sig",
            &ev
        )
        .await
        .unwrap()
    );
    let sent = mock.requests_to("POST", path)[0].json();
    assert_eq!(sent["webhook_id"], "W");
    assert_eq!(sent["webhook_event"], ev);
    assert_eq!(sent["transmission_sig"], "sig");
    mock.on(
        "POST",
        path,
        200,
        json!({ "verification_status": "FAILURE" }),
    );
    assert!(
        !c.verify_webhook_signature("W", "tid", "tt", "u", "a", "s", &ev)
            .await
            .unwrap()
    );
    mock.on("POST", path, 503, json!({}));
    assert!(
        c.verify_webhook_signature("W", "tid", "tt", "u", "a", "s", &ev)
            .await
            .is_err(),
        "galat transport harus dilempar, bukan false"
    );
}

#[test]
fn approval_url_and_total() {
    assert_eq!(get_approval_url(&json!({})), None);
    assert_eq!(
        get_approval_url(&json!({ "links": [{ "rel": "approve", "href": "" }] })),
        None
    );
    assert_eq!(PaypalClient::calculate_total_amount(3.0, 2.5), 7.5);
}
