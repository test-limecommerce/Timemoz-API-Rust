//! Uji integrasi `POST /paypal/webhook` (semua jalur event Node), pelunasan PRORATION,
//! `GET /paypal/proration/return`, dan `GET /paypal/subscription/status` — PayPal TIRUAN.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::*;
use fred::interfaces::KeysInterface;
use mz_testkit::{TestApp, TestResponse};
use serde_json::{Value, json};

const HEADERS: [(&str, &str); 5] = [
    ("paypal-transmission-id", "tx-1"),
    ("paypal-transmission-time", "2026-11-17T00:00:00Z"),
    ("paypal-cert-url", "https://api.paypal.com/cert"),
    ("paypal-auth-algo", "SHA256withRSA"),
    ("paypal-transmission-sig", "sig"),
];

async fn post_event(app: &TestApp, event: &Value) -> TestResponse {
    let mut rb = app.post("/paypal/webhook");
    for (k, v) in HEADERS {
        rb = rb.header(k, v);
    }
    rb.json(event).send().await
}

fn ev(kind: &str, resource: Value) -> (String, Value) {
    let id = format!("WH-{}", uniq());
    let e = json!({ "id": id, "event_type": kind, "resource_type": "subscription", "resource": resource });
    (id, e)
}

/// Kirim event (harus 200) lalu tunggu `processed = true`.
async fn deliver(app: &TestApp, kind: &str, resource: Value) -> String {
    let (id, e) = ev(kind, resource);
    let r = post_event(app, &e).await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json, json!({ "status": "received" }));
    assert!(
        wait_until(
            app,
            &format!(
                r#"SELECT COALESCE((SELECT processed FROM "paypalWebhookEvents" WHERE "eventId" = '{id}'), false)"#
            )
        )
        .await,
        "event {kind} tidak diproses"
    );
    id
}

// ── verifikasi tanda tangan ───────────────────────────────────────────────

#[tokio::test]
async fn signature_rules() {
    let Some((app, mock)) = app().await else {
        return;
    };
    let (id, e) = ev("BILLING.SUBSCRIPTION.EXPIRED", json!({ "id": "I-NOPE" }));

    // Header hilang → 400 tanpa memanggil PayPal.
    for skip in 0..HEADERS.len() {
        let mut rb = app.post("/paypal/webhook");
        for (i, (k, v)) in HEADERS.iter().enumerate() {
            if i != skip {
                rb = rb.header(k, v);
            }
        }
        let r = rb.json(&e).send().await;
        r.assert_status(400);
        assert_eq!(r.json["message"], "Invalid webhook");
    }
    // Event tanpa id → 400.
    let r = post_event(&app, &json!({ "event_type": "X", "resource": {} })).await;
    r.assert_status(400);
    assert!(
        mock.requests_to("POST", "/v1/notifications/verify-webhook-signature")
            .is_empty()
    );

    // Tanda tangan palsu → 400, event TIDAK disimpan.
    mock.on(
        "POST",
        "/v1/notifications/verify-webhook-signature",
        200,
        json!({ "verification_status": "FAILURE" }),
    );
    let r = post_event(&app, &e).await;
    r.assert_status(400);
    assert_eq!(
        r.json,
        json!({ "message": "Invalid webhook signature", "error": "Bad Request", "statusCode": 400 })
    );
    // Body verifikasi: urutan kunci Node + event asli (urutan kunci asli).
    let calls = mock.requests_to("POST", "/v1/notifications/verify-webhook-signature");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].header("authorization"), Some("Bearer tok"));
    let body = &calls[0].body;
    assert!(body.starts_with(r#"{"auth_algo":"SHA256withRSA","cert_url":"https://api.paypal.com/cert","transmission_id":"tx-1","transmission_sig":"sig","transmission_time":"2026-11-17T00:00:00Z","webhook_id":"WH-ID","webhook_event":{"id":""#), "{body}");
    assert!(
        body.contains(
            r#""event_type":"BILLING.SUBSCRIPTION.EXPIRED","resource_type":"subscription""#
        )
    );

    // PayPal galat → 503 (PayPal mengirim ulang).
    mock.on(
        "POST",
        "/v1/notifications/verify-webhook-signature",
        500,
        json!({ "name": "INTERNAL_SERVER_ERROR" }),
    );
    let r = post_event(&app, &e).await;
    r.assert_status(503);
    assert_eq!(
        r.json,
        json!({ "message": "Webhook verification unavailable", "error": "Service Unavailable", "statusCode": 503 })
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let stored: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "paypalWebhookEvents" WHERE "eventId" = '{id}'"#
        ))
        .await;
    assert_eq!(stored, 0);

    // Tanpa PAYPAL_WEBHOOK_ID → 503 tanpa memanggil PayPal.
    let Some((app2, mock2)) = app_with(&[]).await else {
        return;
    };
    let r = post_event(&app2, &e).await;
    r.assert_status(503);
    assert!(mock2.requests().is_empty());
}

#[tokio::test]
async fn duplicate_and_unhandled_events() {
    let Some((app, _mock)) = app().await else {
        return;
    };
    let fx = fixture(&app, STARTER_MONTHLY, false, "").await;
    let (id, e) = ev("BILLING.SUBSCRIPTION.EXPIRED", json!({ "id": fx.pp }));
    post_event(&app, &e).await.assert_status(200);
    assert!(
        wait_until(
            &app,
            &format!(r#"SELECT COALESCE((SELECT processed FROM "paypalWebhookEvents" WHERE "eventId" = '{id}'), false)"#)
        )
        .await
    );
    assert_eq!(sub_row(&app, fx.sub).await["status"], "INACTIVE");
    // Event yang sama dikirim ulang: dilewati (perubahan manual tidak ditimpa).
    app.exec(&format!(
        "UPDATE subscriptions SET status = 'ACTIVE' WHERE id = {}",
        fx.sub
    ))
    .await;
    post_event(&app, &e).await.assert_status(200);
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(sub_row(&app, fx.sub).await["status"], "ACTIVE");
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "paypalWebhookEvents" WHERE "eventId" = '{id}'"#
        ))
        .await;
    assert_eq!(n, 1);
    let (payload, rid): (String, Option<String>) = sqlx::query_as(
        r#"SELECT payload, "resourceId" FROM "paypalWebhookEvents" WHERE "eventId" = $1"#,
    )
    .bind(&id)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(rid.as_deref(), Some(fx.pp.as_str()));
    assert!(payload.starts_with(&format!(r#"{{"id":"{id}","event_type""#)));

    // Event tersimpan tapi belum selesai (processed=false) diproses ulang.
    let (id2, e2) = ev("BILLING.SUBSCRIPTION.EXPIRED", json!({ "id": fx.pp }));
    app.exec(&format!(
        r#"INSERT INTO "paypalWebhookEvents" ("eventId", "eventType", payload) VALUES ('{id2}', 'BILLING.SUBSCRIPTION.EXPIRED', '{{}}')"#
    ))
    .await;
    post_event(&app, &e2).await.assert_status(200);
    assert!(
        wait_until(
            &app,
            &format!(r#"SELECT processed FROM "paypalWebhookEvents" WHERE "eventId" = '{id2}'"#)
        )
        .await
    );
    assert_eq!(sub_row(&app, fx.sub).await["status"], "INACTIVE");

    // Tipe tak dikenal: tetap ditandai processed.
    deliver(&app, "SOMETHING.ELSE", json!({ "id": "x" })).await;
}

// ── BILLING.SUBSCRIPTION.* ────────────────────────────────────────────────

#[tokio::test]
async fn activated_paths() {
    let Some((app, _mock)) = app().await else {
        return;
    };
    // Non-trial: status & quantity, invoice UNPAID → PAID, company.maxUsers.
    let fx = fixture(
        &app,
        STARTER_MONTHLY,
        false,
        r#"status = 'PENDING', "paypalStatus" = NULL"#,
    )
    .await;
    let inv = invoice(&app, fx.sub, "UNPAID", "SUBSCRIPTION", None, 14.97).await;
    deliver(
        &app,
        "BILLING.SUBSCRIPTION.ACTIVATED",
        json!({ "id": fx.pp, "quantity": "3", "status": "ACTIVE" }),
    )
    .await;
    let s = sub_row(&app, fx.sub).await;
    assert_eq!(
        (
            &s["status"],
            &s["paypalStatus"],
            &s["quantity"],
            &s["lastBilledQuantity"],
            &s["autoRenew"]
        ),
        (
            &json!("ACTIVE"),
            &json!("ACTIVE"),
            &json!(3),
            &json!(3),
            &json!(true)
        )
    );
    assert_eq!(invoice_status(&app, inv).await, "PAID");
    assert_eq!(company_row(&app, fx.company).await["maxUsers"], 3);
    // Audit `company.update` ditulis sebagai system.
    let audits: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "auditLog" WHERE "entityType" = 'company' AND "entityId" = {} AND "actorType" = 'system'"#,
            fx.company
        ))
        .await;
    assert!(audits >= 1);

    // Trial: invoice tetap UNPAID, analytics upgrade_to_pro (trial → tier berbayar).
    let fx = fixture(&app, STARTER_MONTHLY, true, r#""autoRenew" = false"#).await;
    let inv = invoice(&app, fx.sub, "UNPAID", "SUBSCRIPTION", None, 4.99).await;
    deliver(
        &app,
        "BILLING.SUBSCRIPTION.ACTIVATED",
        json!({ "id": fx.pp }),
    )
    .await;
    assert_eq!(invoice_status(&app, inv).await, "UNPAID");
    let s = sub_row(&app, fx.sub).await;
    // quantity tanpa resource → lokal (2); autoRenew dibaca ulang (false dipertahankan).
    assert_eq!(
        (&s["quantity"], &s["autoRenew"]),
        (&json!(2), &json!(false))
    );
    assert!(
        wait_until(
            &app,
            &format!(
                r#"SELECT EXISTS (SELECT 1 FROM "analyticsEvent" WHERE "userId" = {} AND "eventName" = 'upgrade_to_pro'
                     AND params->>'payment_provider' = 'paypal')"#,
                fx.owner
            )
        )
        .await
    );

    // Langganan tertunda (pendingPaypalSubscriptionId) → tidak menulis apa pun.
    let fx2 = fixture(&app, STARTER_MONTHLY, false, "").await;
    let pend = format!("I-PEND{}", uniq()[..8].to_uppercase());
    app.exec(&format!(
        r#"UPDATE subscriptions SET "pendingPaypalSubscriptionId" = '{pend}', status = 'PENDING' WHERE id = {}"#,
        fx2.sub
    ))
    .await;
    deliver(
        &app,
        "BILLING.SUBSCRIPTION.ACTIVATED",
        json!({ "id": pend }),
    )
    .await;
    assert_eq!(sub_row(&app, fx2.sub).await["status"], "PENDING");

    // MANUAL → diabaikan, event tetap processed.
    let fx3 = fixture(
        &app,
        STARTER_MONTHLY,
        false,
        r#""billingSource" = 'MANUAL', status = 'PENDING'"#,
    )
    .await;
    deliver(
        &app,
        "BILLING.SUBSCRIPTION.ACTIVATED",
        json!({ "id": fx3.pp }),
    )
    .await;
    assert_eq!(sub_row(&app, fx3.sub).await["status"], "PENDING");

    // Tidak ditemukan → no-op.
    deliver(
        &app,
        "BILLING.SUBSCRIPTION.ACTIVATED",
        json!({ "id": "I-UNKNOWN0000" }),
    )
    .await;
}

#[tokio::test]
async fn cancelled_suspended_expired() {
    let Some((app, _mock)) = app().await else {
        return;
    };
    // Cancel tak terduga (autoRenew=true) → CANCELLED + autoRenew=false.
    let a = fixture(&app, STARTER_MONTHLY, false, "").await;
    deliver(
        &app,
        "BILLING.SUBSCRIPTION.CANCELLED",
        json!({ "id": a.pp }),
    )
    .await;
    let s = sub_row(&app, a.sub).await;
    assert_eq!(
        (&s["status"], &s["paypalStatus"], &s["autoRenew"]),
        (&json!("CANCELLED"), &json!("CANCELLED"), &json!(false))
    );
    // Cancel yang diharapkan (autoRenew=false) → status tetap.
    let b = fixture(&app, STARTER_MONTHLY, false, r#""autoRenew" = false"#).await;
    deliver(
        &app,
        "BILLING.SUBSCRIPTION.CANCELLED",
        json!({ "id": b.pp }),
    )
    .await;
    let s = sub_row(&app, b.sub).await;
    assert_eq!(
        (&s["status"], &s["paypalStatus"]),
        (&json!("ACTIVE"), &json!("CANCELLED"))
    );
    // Cancel karena ganti plan (pending) → status tetap meski autoRenew=true.
    let c = fixture(
        &app,
        STARTER_MONTHLY,
        false,
        r#""pendingPaypalSubscriptionId" = 'I-NEWPLAN'"#,
    )
    .await;
    deliver(
        &app,
        "BILLING.SUBSCRIPTION.CANCELLED",
        json!({ "id": c.pp }),
    )
    .await;
    let s = sub_row(&app, c.sub).await;
    assert_eq!(
        (&s["status"], &s["autoRenew"]),
        (&json!("ACTIVE"), &json!(true))
    );

    // Suspend anggun vs tak terduga.
    let d = fixture(&app, STARTER_MONTHLY, false, r#""autoRenew" = false"#).await;
    deliver(
        &app,
        "BILLING.SUBSCRIPTION.SUSPENDED",
        json!({ "id": d.pp }),
    )
    .await;
    let s = sub_row(&app, d.sub).await;
    assert_eq!(
        (&s["status"], &s["paypalStatus"]),
        (&json!("ACTIVE"), &json!("SUSPENDED"))
    );
    let e = fixture(&app, STARTER_MONTHLY, false, "").await;
    deliver(
        &app,
        "BILLING.SUBSCRIPTION.SUSPENDED",
        json!({ "id": e.pp }),
    )
    .await;
    assert_eq!(sub_row(&app, e.sub).await["status"], "SUSPENDED");

    // Expired.
    let f = fixture(&app, STARTER_MONTHLY, false, "").await;
    deliver(&app, "BILLING.SUBSCRIPTION.EXPIRED", json!({ "id": f.pp })).await;
    let s = sub_row(&app, f.sub).await;
    assert_eq!(
        (&s["status"], &s["paypalStatus"]),
        (&json!("INACTIVE"), &json!("EXPIRED"))
    );

    // MANUAL: tiga event di atas tidak menulis.
    let m = fixture(
        &app,
        STARTER_MONTHLY,
        false,
        r#""billingSource" = 'MANUAL'"#,
    )
    .await;
    for kind in [
        "BILLING.SUBSCRIPTION.CANCELLED",
        "BILLING.SUBSCRIPTION.SUSPENDED",
        "BILLING.SUBSCRIPTION.EXPIRED",
        "BILLING.SUBSCRIPTION.UPDATED",
    ] {
        deliver(&app, kind, json!({ "id": m.pp, "quantity": "9" })).await;
    }
    let s = sub_row(&app, m.sub).await;
    assert_eq!(
        (&s["status"], &s["quantity"]),
        (&json!("ACTIVE"), &json!(2))
    );
}

#[tokio::test]
async fn updated_paths() {
    let Some((app, _mock)) = app().await else {
        return;
    };
    // Update biasa: quantity/status/plan; invoice terbaru TIDAK jadi PAID (perbaikan Rust, deviasi) + link approve.
    let fx = fixture(&app, STARTER_MONTHLY, false, "").await;
    let inv = invoice(&app, fx.sub, "UNPAID", "SUBSCRIPTION", None, 9.98).await;
    deliver(
        &app,
        "BILLING.SUBSCRIPTION.UPDATED",
        json!({ "id": fx.pp, "quantity": "5", "status": "ACTIVE", "plan_id": "P-NEW",
                "links": [{ "rel": "approve", "href": "https://pp.example/approve" }] }),
    )
    .await;
    let s = sub_row(&app, fx.sub).await;
    assert_eq!(
        (&s["quantity"], &s["paypalPlanId"], &s["paypalStatus"]),
        (&json!(5), &json!("P-NEW"), &json!("ACTIVE"))
    );
    assert_eq!(company_row(&app, fx.company).await["maxUsers"], 5);
    let (st, url, pid): (String, String, Option<String>) = sqlx::query_as(
        r#"SELECT status::text, "paymentUrl", "paypalPaymentId" FROM invoices WHERE id = $1"#,
    )
    .bind(inv)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(
        (st.as_str(), url.as_str(), pid.as_deref()),
        ("UNPAID", "https://pp.example/approve", Some(fx.pp.as_str()))
    );

    // Revisi plan tertunda di-approve → tier ditukar, kolom company dari tier.
    let fx = fixture(
        &app,
        STARTER_MONTHLY,
        false,
        &format!(
            r#""pendingPaypalPlanId" = 'P-GROWTH', "pendingPricingTierId" = {GROWTH_MONTHLY}"#
        ),
    )
    .await;
    deliver(
        &app,
        "BILLING.SUBSCRIPTION.UPDATED",
        json!({ "id": fx.pp, "plan_id": "P-GROWTH", "status": "ACTIVE" }),
    )
    .await;
    let s = sub_row(&app, fx.sub).await;
    assert_eq!(
        (
            &s["pricingTierId"],
            &s["pendingPricingTierId"],
            &s["paypalPlanId"]
        ),
        (
            &json!(GROWTH_MONTHLY),
            &Value::Null,
            &json!("P-3SU39545NH977270XNGHIBPY")
        )
    );
    let tier_max: i32 = app
        .scalar(&format!(
            r#"SELECT "maxUsers" FROM "pricingTiers" WHERE id = {GROWTH_MONTHLY}"#
        ))
        .await;
    assert_eq!(company_row(&app, fx.company).await["maxUsers"], tier_max);
}

// ── PAYMENT.SALE.* ────────────────────────────────────────────────────────

#[tokio::test]
async fn sale_completed_trial_and_renewal() {
    let Some((app, _mock)) = app().await else {
        return;
    };
    // Trial → berbayar, pembayaran pertama: invoice PAID + item dibuat, isTrial=false.
    let fx = fixture(&app, STARTER_YEARLY, true, r#""billingInterval" = 'YEAR'"#).await;
    deliver(
        &app,
        "PAYMENT.SALE.COMPLETED",
        json!({ "id": "SALE-1", "billing_agreement_id": fx.pp }),
    )
    .await;
    let s = sub_row(&app, fx.sub).await;
    assert_eq!(
        (&s["status"], &s["nextInvoiceSet"]),
        (&json!("ACTIVE"), &json!(true))
    );
    let days = s["endInDays"].as_f64().unwrap();
    assert!((364.0..=366.0).contains(&days), "{s}");
    let c = company_row(&app, fx.company).await;
    assert_eq!((&c["isTrial"], &c["maxUsers"]), (&json!(false), &json!(2)));
    let (num, total, pid, desc, unit): (String, f64, Option<String>, String, f64) = sqlx::query_as(
        r#"SELECT i."invoiceNumber", i."totalPrice", i."paypalPaymentId", it.description, it."unitPrice"
             FROM invoices i JOIN "invoiceItems" it ON it."invoiceId" = i.id
            WHERE i."subscriptionId" = $1 AND i.status = 'PAID'"#,
    )
    .bind(fx.sub)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert!(num.starts_with("INV-2"), "{num}");
    assert_eq!(pid.as_deref(), Some("SALE-1"));
    // basePrice 2.99 × 12 × 2 user.
    assert!((unit - 2.99 * 12.0).abs() < 1e-9);
    assert!((total - 2.99 * 12.0 * 2.0).abs() < 1e-9);
    assert_eq!(
        desc,
        "Yearly Subscription - 2 user(s) @ $2.99/user/month (billed yearly)"
    );

    // Renewal dalam siklus tanpa invoice tertunda → dilewati.
    let fx = fixture(&app, STARTER_MONTHLY, false, "").await;
    deliver(
        &app,
        "PAYMENT.SALE.COMPLETED",
        json!({ "id": "SALE-2", "billing_agreement_id": fx.pp }),
    )
    .await;
    assert_eq!(sub_row(&app, fx.sub).await["endInDays"], 5);
    // Dengan invoice UNPAID → PAID, tanggal diperpanjang 30 hari.
    let inv = invoice(&app, fx.sub, "UNPAID", "SUBSCRIPTION", None, 9.98).await;
    deliver(
        &app,
        "PAYMENT.SALE.COMPLETED",
        json!({ "id": "SALE-3", "billing_agreement_id": fx.pp }),
    )
    .await;
    assert_eq!(invoice_status(&app, inv).await, "PAID");
    assert_eq!(sub_row(&app, fx.sub).await["endInDays"], 30);

    // Invoice PRORATION tertunda → hanya PAID, tanggal tidak berubah.
    let fx = fixture(&app, STARTER_MONTHLY, false, "").await;
    let pr = invoice(&app, fx.sub, "UNPAID", "PRORATION", None, 3.0).await;
    deliver(
        &app,
        "PAYMENT.SALE.COMPLETED",
        json!({ "id": "SALE-4", "billing_agreement_id": fx.pp }),
    )
    .await;
    assert_eq!(invoice_status(&app, pr).await, "PAID");
    assert_eq!(sub_row(&app, fx.sub).await["endInDays"], 5);

    // Tanpa billing_agreement_id → no-op.
    deliver(&app, "PAYMENT.SALE.COMPLETED", json!({ "id": "SALE-5" })).await;
}

#[tokio::test]
async fn sale_completed_pending_plan_change() {
    let Some((app, mock)) = app().await else {
        return;
    };
    mock.on("POST", "/v1/billing/subscriptions/*", 204, Value::Null);
    let new_pp = format!("I-NEW{}", uniq()[..9].to_uppercase());
    let fx = fixture(
        &app,
        STARTER_MONTHLY,
        false,
        &format!(
            r#""pendingPaypalSubscriptionId" = '{new_pp}', "pendingPaypalPlanId" = 'P-G', "pendingPricingTierId" = {GROWTH_MONTHLY}"#
        ),
    )
    .await;
    let inv = invoice(&app, fx.sub, "UNPAID", "SUBSCRIPTION", None, 15.0).await;
    // Cache user-me pemilik harus dihapus.
    let key = format!("user-me-{}", fx.owner);
    app.state()
        .redis
        .next()
        .set::<(), _, _>(&key, "cached", None, None, false)
        .await
        .unwrap();
    deliver(
        &app,
        "PAYMENT.SALE.COMPLETED",
        json!({ "id": "SALE-P", "billing_agreement_id": new_pp }),
    )
    .await;
    let s = sub_row(&app, fx.sub).await;
    assert_eq!(
        (
            &s["paypalSubscriptionId"],
            &s["paypalPlanId"],
            &s["pricingTierId"],
            &s["pendingPaypalSubscriptionId"],
            &s["status"]
        ),
        (
            &json!(new_pp),
            &json!("P-G"),
            &json!(GROWTH_MONTHLY),
            &Value::Null,
            &json!("ACTIVE")
        )
    );
    assert_eq!(s["endInDays"], 30);
    assert_eq!(invoice_status(&app, inv).await, "PAID");
    // Langganan LAMA dibatalkan di PayPal (status ACTIVE).
    let cancel = mock.requests_to(
        "POST",
        &format!("/v1/billing/subscriptions/{}/cancel", fx.pp),
    );
    assert_eq!(cancel.len(), 1);
    assert_eq!(
        cancel[0].json(),
        json!({ "reason": "Plan changed — old subscription replaced after payment" })
    );
    assert_eq!(
        company_row(&app, fx.company).await["reportExportsUsedThisMonth"],
        0
    );
    let exists: i64 = app.state().redis.next().exists(&key).await.unwrap();
    assert_eq!(exists, 0);

    // Tanpa invoice UNPAID → invoice cadangan PAID dibuat; status lama SUSPENDED juga dibatalkan,
    // CANCELLED tidak.
    let new2 = format!("I-NEW{}", uniq()[..9].to_uppercase());
    let fx = fixture(
        &app,
        STARTER_MONTHLY,
        false,
        &format!(
            r#""pendingPaypalSubscriptionId" = '{new2}', "pendingPricingTierId" = {GROWTH_MONTHLY}, "paypalStatus" = 'CANCELLED'"#
        ),
    )
    .await;
    mock.clear_log();
    deliver(
        &app,
        "PAYMENT.SALE.COMPLETED",
        json!({ "id": "SALE-Q", "billing_agreement_id": new2 }),
    )
    .await;
    assert!(
        mock.requests_to(
            "POST",
            &format!("/v1/billing/subscriptions/{}/cancel", fx.pp)
        )
        .is_empty()
    );
    let (desc, total): (String, f64) = sqlx::query_as(
        r#"SELECT it.description, i."totalPrice" FROM invoices i JOIN "invoiceItems" it ON it."invoiceId" = i.id
            WHERE i."subscriptionId" = $1 AND i.status = 'PAID'"#,
    )
    .bind(fx.sub)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(
        desc,
        "Monthly Subscription (plan change to Growth Monthly) - 2 user(s) @ $7.5/user"
    );
    assert!((total - 15.0).abs() < 1e-9);
}

#[tokio::test]
async fn sale_denied_and_refunded() {
    let Some((app, _mock)) = app().await else {
        return;
    };
    for kind in ["PAYMENT.SALE.DENIED", "PAYMENT.SALE.REFUNDED"] {
        let fx = fixture(&app, STARTER_MONTHLY, false, "").await;
        deliver(
            &app,
            kind,
            json!({ "id": "S", "billing_agreement_id": fx.pp }),
        )
        .await;
        let s = sub_row(&app, fx.sub).await;
        assert_eq!(
            (&s["status"], &s["paypalStatus"]),
            (&json!("SUSPENDED"), &json!("PAYMENT_FAILED"))
        );
    }
    let m = fixture(
        &app,
        STARTER_MONTHLY,
        false,
        r#""billingSource" = 'MANUAL'"#,
    )
    .await;
    deliver(
        &app,
        "PAYMENT.SALE.DENIED",
        json!({ "billing_agreement_id": m.pp }),
    )
    .await;
    assert_eq!(sub_row(&app, m.sub).await["status"], "ACTIVE");
}

// ── PRORATION (order) ─────────────────────────────────────────────────────

fn order(id: &str, status: &str, cap_status: &str, value: &str) -> Value {
    json!({ "id": id, "status": status, "purchase_units": [{ "payments": { "captures": [
        { "id": "CAP-1", "status": cap_status, "amount": { "currency_code": "USD", "value": value } }
    ] } }] })
}

fn order_id() -> String {
    format!("ORD{}", uniq()[..14].to_uppercase())
}

#[tokio::test]
async fn order_approved_and_capture_completed() {
    let Some((app, mock)) = app().await else {
        return;
    };
    // APPROVED: capture → PAID.
    let fx = fixture(&app, STARTER_MONTHLY, false, "").await;
    let oid = order_id();
    let inv = invoice(&app, fx.sub, "UNPAID", "PRORATION", Some(&oid), 12.5).await;
    mock.on(
        "POST",
        &format!("/v2/checkout/orders/{oid}/capture"),
        201,
        order(&oid, "COMPLETED", "COMPLETED", "12.50"),
    );
    deliver(&app, "CHECKOUT.ORDER.APPROVED", json!({ "id": oid })).await;
    assert_eq!(invoice_status(&app, inv).await, "PAID");

    // CAPTURE.COMPLETED atas invoice EXPIRED: user yang dinonaktifkan cron diaktifkan lagi,
    // seat ditambah sebanyak item.
    let fx = fixture(&app, STARTER_MONTHLY, false, "").await;
    let member: i32 = app
        .scalar(&format!(
            r#"WITH u AS (INSERT INTO users (username, password, name, status)
                          VALUES ('it-pp-m-{}@example.com', 'x', 'M', 1) RETURNING id)
               INSERT INTO "userCompany" ("userId", "companyId", status, "idleTimeInSecond", "isDefault")
               SELECT id, {}, 0, 180, true FROM u RETURNING id"#,
            uniq(),
            fx.company
        ))
        .await;
    let oid = order_id();
    let inv = invoice(&app, fx.sub, "EXPIRED", "PRORATION", Some(&oid), 4.99).await;
    app.exec(&format!(
        r#"UPDATE invoices SET "prorationDeactivatedUserCompanyIds" = ARRAY[{member}] WHERE id = {inv};
           INSERT INTO "invoiceItems" ("invoiceId", description, quantity, "unitPrice", "totalPrice")
           VALUES ({inv}, 'Prorated seat', 1, 4.99, 4.99);"#
    ))
    .await;
    mock.on(
        "GET",
        &format!("/v2/checkout/orders/{oid}"),
        200,
        order(&oid, "COMPLETED", "COMPLETED", "4.99"),
    );
    deliver(
        &app,
        "PAYMENT.CAPTURE.COMPLETED",
        json!({ "id": "CAP-9", "supplementary_data": { "related_ids": { "order_id": oid } } }),
    )
    .await;
    assert_eq!(invoice_status(&app, inv).await, "PAID");
    let st: i32 = app
        .scalar(&format!(
            r#"SELECT status FROM "userCompany" WHERE id = {member}"#
        ))
        .await;
    assert_eq!(st, 1);
    let s = sub_row(&app, fx.sub).await;
    assert_eq!(
        (&s["quantity"], &s["lastBilledQuantity"]),
        (&json!(3), &json!(1))
    );
    // Audit userCompany ditulis sebagai system/proration-payment.
    let audits: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "auditLog" WHERE "actorName" = 'proration-payment' AND "companyId" = {}"#,
            fx.company
        ))
        .await;
    assert!(audits >= 1);
    // Tanpa capture ulang.
    assert!(
        mock.requests_to("POST", &format!("/v2/checkout/orders/{oid}/capture"))
            .is_empty()
    );

    // MANUAL: tanpa capture/apply.
    let m = fixture(
        &app,
        STARTER_MONTHLY,
        false,
        r#""billingSource" = 'MANUAL'"#,
    )
    .await;
    let oid = order_id();
    let inv = invoice(&app, m.sub, "UNPAID", "PRORATION", Some(&oid), 1.0).await;
    deliver(&app, "CHECKOUT.ORDER.APPROVED", json!({ "id": oid })).await;
    assert_eq!(invoice_status(&app, inv).await, "UNPAID");
    assert!(
        mock.requests_to("POST", &format!("/v2/checkout/orders/{oid}/capture"))
            .is_empty()
    );

    // Order tanpa id terkait → no-op.
    deliver(&app, "PAYMENT.CAPTURE.COMPLETED", json!({ "id": "CAP-X" })).await;
}

async fn proration_return(app: &TestApp, token: &str) -> TestResponse {
    app.get(&format!("/paypal/proration/return?token={token}&PayerID=X"))
        .send()
        .await
}

fn location(r: &TestResponse) -> String {
    r.headers
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn proration_return_route() {
    let Some((app, mock)) = app().await else {
        return;
    };
    let fx = fixture(&app, STARTER_MONTHLY, false, "").await;

    // Sukses → success + nomor invoice; panggilan kedua (sudah PAID) tetap success.
    let oid = order_id();
    let inv = invoice(&app, fx.sub, "UNPAID", "PRORATION", Some(&oid), 7.5).await;
    let number: String = app
        .scalar(&format!(
            r#"SELECT "invoiceNumber" FROM invoices WHERE id = {inv}"#
        ))
        .await;
    mock.on(
        "POST",
        &format!("/v2/checkout/orders/{oid}/capture"),
        201,
        order(&oid, "COMPLETED", "COMPLETED", "7.50"),
    );
    let r = proration_return(&app, &oid).await;
    r.assert_status(302);
    assert_eq!(
        location(&r),
        format!("https://app.example.test/billing?proration=success&invoice={number}")
    );
    assert_eq!(r.text(), format!("Found. Redirecting to {}", location(&r)));
    assert_eq!(invoice_status(&app, inv).await, "PAID");
    let r = proration_return(&app, &oid).await;
    assert!(location(&r).contains("proration=success"));
    assert_eq!(
        mock.requests_to("POST", &format!("/v2/checkout/orders/{oid}/capture"))
            .len(),
        1
    );

    // Token tidak sah → failed tanpa memanggil PayPal.
    for bad in ["5o190127tn364715t", "ABC123", ""] {
        let r = proration_return(&app, bad).await;
        r.assert_status(302);
        assert_eq!(
            location(&r),
            "https://app.example.test/billing?proration=failed"
        );
    }
    let r = app
        .get(&format!("/paypal/proration/return?token={oid}&token={oid}"))
        .send()
        .await;
    assert_eq!(
        location(&r),
        "https://app.example.test/billing?proration=failed"
    );

    // Belum di-approve (422 ORDER_NOT_APPROVED) → pending, invoice tetap UNPAID.
    let oid = order_id();
    let inv = invoice(&app, fx.sub, "UNPAID", "PRORATION", Some(&oid), 2.0).await;
    mock.on(
        "POST",
        &format!("/v2/checkout/orders/{oid}/capture"),
        422,
        json!({ "name": "UNPROCESSABLE_ENTITY", "details": [{ "issue": "ORDER_NOT_APPROVED" }] }),
    );
    let r = proration_return(&app, &oid).await;
    assert!(
        location(&r).starts_with("https://app.example.test/billing?proration=pending&invoice=")
    );
    assert_eq!(invoice_status(&app, inv).await, "UNPAID");

    // Sudah di-capture (ORDER_ALREADY_CAPTURED) → GET order; nominal beda → failed.
    mock.on(
        "POST",
        &format!("/v2/checkout/orders/{oid}/capture"),
        422,
        json!({ "details": [{ "issue": "ORDER_ALREADY_CAPTURED" }] }),
    );
    mock.on(
        "GET",
        &format!("/v2/checkout/orders/{oid}"),
        200,
        order(&oid, "COMPLETED", "COMPLETED", "2.01"),
    );
    let r = proration_return(&app, &oid).await;
    assert!(location(&r).contains("proration=failed&invoice="));
    assert_eq!(invoice_status(&app, inv).await, "UNPAID");

    // Galat PayPal lain → failed tanpa invoice.
    mock.on(
        "POST",
        &format!("/v2/checkout/orders/{oid}/capture"),
        500,
        json!({ "name": "INTERNAL_SERVER_ERROR" }),
    );
    let r = proration_return(&app, &oid).await;
    assert_eq!(
        location(&r),
        "https://app.example.test/billing?proration=failed"
    );

    // Invoice CANCELLED → failed tanpa capture.
    let oid = order_id();
    invoice(&app, fx.sub, "CANCELLED", "PRORATION", Some(&oid), 2.0).await;
    let r = proration_return(&app, &oid).await;
    assert!(location(&r).contains("proration=failed&invoice="));
    assert!(
        mock.requests_to("POST", &format!("/v2/checkout/orders/{oid}/capture"))
            .is_empty()
    );

    // EXPIRED + subscription tidak ACTIVE → PAID tanpa reaktivasi (success, refund manual).
    let g = fixture(&app, STARTER_MONTHLY, false, "status = 'SUSPENDED'").await;
    let oid = order_id();
    let inv = invoice(&app, g.sub, "EXPIRED", "PRORATION", Some(&oid), 3.0).await;
    mock.on(
        "POST",
        &format!("/v2/checkout/orders/{oid}/capture"),
        201,
        order(&oid, "COMPLETED", "COMPLETED", "3.00"),
    );
    let r = proration_return(&app, &oid).await;
    assert!(location(&r).contains("proration=success"));
    assert_eq!(invoice_status(&app, inv).await, "PAID");
    assert_eq!(sub_row(&app, g.sub).await["quantity"], 2);

    // Capture PENDING → pending.
    let oid = order_id();
    invoice(&app, fx.sub, "UNPAID", "PRORATION", Some(&oid), 3.0).await;
    mock.on(
        "POST",
        &format!("/v2/checkout/orders/{oid}/capture"),
        201,
        order(&oid, "COMPLETED", "PENDING", "3.00"),
    );
    assert!(location(&proration_return(&app, &oid).await).contains("proration=pending"));

    // Order tanpa invoice → failed.
    assert_eq!(
        location(&proration_return(&app, &order_id()).await),
        "https://app.example.test/billing?proration=failed"
    );
}

// ── GET /paypal/subscription/status ───────────────────────────────────────

#[tokio::test]
async fn subscription_status_route() {
    let Some((app, mock)) = app().await else {
        return;
    };
    let fx = fixture(&app, STARTER_MONTHLY, false, "").await;
    let token = app.token(fx.owner);
    let url = |id: &str| format!("/paypal/subscription/status?subscription_id={id}");

    // 401 tanpa token.
    app.get(&url(&fx.pp)).send().await.assert_status(401);

    // Tanpa invoice.
    let r = app.get(&url(&fx.pp)).token(&token).send().await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(
        r.json,
        json!({ "success": false, "message": "Invoice not found for this subscription" })
    );

    // Invoice PAID → data lengkap (+subscription), paypalStatus null, tanpa panggilan PayPal.
    let inv = invoice(&app, fx.sub, "PAID", "SUBSCRIPTION", None, 9.98).await;
    let r = app.get(&url(&fx.pp)).token(&token).send().await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.json["message"], "Success");
    assert_eq!(r.json["paypalStatus"], Value::Null);
    let data = r.data();
    assert_eq!(data["id"], inv);
    assert_eq!(data["totalPrice"], json!(9.98));
    assert_eq!(data["subPrice"], json!(9.98));
    assert_eq!(data["feePrice"], json!(0));
    assert_eq!(data["prorationDeactivatedUserCompanyIds"], json!([]));
    assert_eq!(data["subscription"]["id"], fx.sub);
    assert_eq!(data["subscription"]["pricePerUser"], json!(4.99));
    let keys: Vec<&str> = data
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys.first(), Some(&"id"));
    assert_eq!(keys.last(), Some(&"subscription"));
    assert_eq!(keys.len(), 27);
    assert_eq!(data["subscription"].as_object().unwrap().len(), 31);
    assert!(data["createdAt"].as_str().unwrap().ends_with('Z'));
    assert!(
        mock.requests_to("GET", &format!("/v1/billing/subscriptions/{}", fx.pp))
            .is_empty()
    );

    // UNPAID + PayPal ACTIVE → long poll; webhook menandai PAID di tengah jalan.
    app.exec(&format!(
        "UPDATE invoices SET status = 'UNPAID' WHERE id = {inv}"
    ))
    .await;
    mock.on(
        "GET",
        &format!("/v1/billing/subscriptions/{}", fx.pp),
        200,
        json!({ "id": fx.pp, "status": "ACTIVE" }),
    );
    let db = app.db().clone();
    let flip = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        sqlx::query("UPDATE invoices SET status = 'PAID' WHERE id = $1")
            .bind(inv)
            .execute(&db)
            .await
            .unwrap();
    });
    let r = app.get(&url(&fx.pp)).token(&token).send().await;
    flip.await.unwrap();
    r.assert_ok();
    assert_eq!(
        (r.json["paypalStatus"].clone(), r.data()["status"].clone()),
        (json!("ACTIVE"), json!("PAID"))
    );

    // UNPAID + PayPal APPROVAL_PENDING → tanpa long poll.
    app.exec(&format!(
        "UPDATE invoices SET status = 'UNPAID' WHERE id = {inv}"
    ))
    .await;
    mock.on(
        "GET",
        &format!("/v1/billing/subscriptions/{}", fx.pp),
        200,
        json!({ "status": "APPROVAL_PENDING" }),
    );
    let r = app.get(&url(&fx.pp)).token(&token).send().await;
    assert_eq!(
        (r.json["paypalStatus"].clone(), r.data()["status"].clone()),
        (json!("APPROVAL_PENDING"), json!("UNPAID"))
    );
    // PayPal galat → paypalStatus null.
    mock.on(
        "GET",
        &format!("/v1/billing/subscriptions/{}", fx.pp),
        500,
        json!({}),
    );
    let r = app.get(&url(&fx.pp)).token(&token).send().await;
    assert_eq!(r.json["paypalStatus"], Value::Null);

    // Company lain.
    let other = fixture(&app, STARTER_MONTHLY, false, "").await;
    let r = app.get(&url(&other.pp)).token(&token).send().await;
    assert_eq!(
        r.json,
        json!({ "success": false, "message": "Unauthorized: subscription does not belong to your company" })
    );
    // Tidak ditemukan.
    let r = app.get(&url("I-NONE")).token(&token).send().await;
    assert_eq!(
        r.json,
        json!({ "success": false, "message": "Subscription not found" })
    );

    // Plan change tertunda tanpa invoice UNPAID → dijadwalkan.
    let pend = format!("I-PEND{}", uniq()[..8].to_uppercase());
    app.exec(&format!(
        r#"UPDATE subscriptions SET "pendingPaypalSubscriptionId" = '{pend}' WHERE id = {}"#,
        fx.sub
    ))
    .await;
    app.exec(&format!(
        "UPDATE invoices SET status = 'PAID' WHERE id = {inv}"
    ))
    .await;
    let r = app.get(&url(&pend)).token(&token).send().await;
    r.assert_ok();
    let keys: Vec<&str> = r
        .json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "success",
            "message",
            "data",
            "isPendingPlanChange",
            "scheduledAt",
            "paypalStatus"
        ]
    );
    assert_eq!(r.json["message"], "Plan change scheduled");

    // User tanpa company.
    let lone: i32 = app
        .scalar(&format!(
            r#"INSERT INTO users (username, password, name, status) VALUES ('it-pp-lone-{}@example.com', 'x', 'L', 1) RETURNING id"#,
            uniq()
        ))
        .await;
    let r = app.get(&url(&fx.pp)).token(&app.token(lone)).send().await;
    assert_eq!(
        r.json,
        json!({ "success": false, "message": "User company not found" })
    );
}
