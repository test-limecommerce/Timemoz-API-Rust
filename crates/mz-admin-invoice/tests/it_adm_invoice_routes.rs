//! Uji integrasi `admin-invoices` (operator) terhadap Postgres nyata
//! (`mz_test_it_adm_invoice_routes`). Email tidak pernah dikirim (tanpa `MAILTRAP_TOKEN`
//! pengiriman gagal → jalur 502 Node). Jalankan:
//! `MZ_TEST_DATABASE_URL=... cargo test -p mz-admin-invoice --test it_adm_invoice_routes`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use mz_core::AppState;
use mz_testkit::TestApp;
use serde_json::{Value, json};

fn build(state: &AppState) -> Router {
    mz_admin_invoice::router(state).with_state(state.clone())
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

async fn operator(app: &TestApp, role: &str) -> (i32, String) {
    let u = uniq();
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO operators (username, password, name, status, role)
               VALUES ('op{u}', 'x', 'Finance Op', 1, '{role}') RETURNING id"#
        ))
        .await;
    (id, app.token_with(json!({ "sub": id })))
}

struct Fx {
    company_id: i32,
    company_uuid: String,
    tag: String,
    manual_paid: String,
    manual_unpaid: String,
    paypal: String,
    cancelled: String,
}

/// Company + subscription + 4 invoice (nomor ber-tag unik supaya filter `search` terisolasi).
async fn fixture(app: &TestApp, op: i32, owner: Option<i32>) -> Fx {
    let tag = uniq()[..10].to_uppercase();
    let (company_id, company_uuid): (i32, String) = sqlx::query_as(
        r#"INSERT INTO company (name, status, "profilePictureFullPath", "ownerUserId")
           VALUES ($1, 1, 'logo/key.png', $2) RETURNING id, uuid::text"#,
    )
    .bind(format!("it-adm-inv-{tag}"))
    .bind(owner)
    .fetch_one(app.db())
    .await
    .unwrap();
    let sub: i32 = app
        .scalar(&format!(
            r#"INSERT INTO subscriptions ("companyId", "pricingTierId", "startDate", "endDate", status)
               VALUES ({company_id}, (SELECT MIN(id) FROM "pricingTiers"), '2026-11-01', '2026-12-01', 'ACTIVE')
               RETURNING id"#
        ))
        .await;
    let mk = |num: &str, status: &str, provider: &str, created: &str, op: Option<i32>| {
        format!(
            r#"INSERT INTO invoices ("subscriptionId", "invoiceNumber", "paymentUrl", "totalPrice", status,
                                     "paymentProvider", "createdAt", "expiredAt", "recordedByOperatorId", currency)
               VALUES ({sub}, 'INV-{tag}-{num}', 'https://pay/{num}', 150.5, '{status}', '{provider}',
                       '{created}', '2026-12-01', {}, 'IDR') RETURNING uuid::text"#,
            op.map_or("NULL".to_owned(), |o| o.to_string())
        )
    };
    let manual_paid: String = app
        .scalar(&mk("A", "PAID", "manual", "2026-11-02 10:00:00", Some(op)))
        .await;
    let manual_unpaid: String = app
        .scalar(&mk("B", "UNPAID", "manual", "2026-11-03 23:59:59", None))
        .await;
    let paypal: String = app
        .scalar(&mk("C", "PAID", "paypal", "2026-11-04 00:00:00", None))
        .await;
    let cancelled: String = app
        .scalar(&mk("D", "CANCELLED", "manual", "2026-11-05 00:00:00", None))
        .await;
    Fx {
        company_id,
        company_uuid,
        tag,
        manual_paid,
        manual_unpaid,
        paypal,
        cancelled,
    }
}

#[tokio::test]
async fn gerbang_dan_validasi() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let (_, support) = operator(&app, "SUPPORT").await;
    let fx = app.company().await;
    let u = uuid::Uuid::new_v4();
    for (m, uri) in [
        (http::Method::GET, "/admin-invoices".to_owned()),
        (http::Method::GET, format!("/admin-invoices/{u}")),
        (http::Method::POST, format!("/admin-invoices/{u}/void")),
        (
            http::Method::POST,
            format!("/admin-invoices/{u}/resend-email"),
        ),
    ] {
        let res = app.request(m.clone(), &uri).send().await;
        res.assert_status(401);
        assert_eq!(
            res.json,
            json!({ "message": "Unauthorized", "statusCode": 401 })
        );
        app.request(m, &uri)
            .user(fx.super_admin.id)
            .send()
            .await
            .assert_status(401);
    }
    for uri in [
        format!("/admin-invoices/{u}/void"),
        format!("/admin-invoices/{u}/resend-email"),
    ] {
        let res = app.post(&uri).token(&support).json(&json!({})).send().await;
        res.assert_status(403);
        assert_eq!(res.json["code"], "OPERATOR_ROLE_FORBIDDEN");
    }
    // Daftar & detail hanya `JwtAdminAuthGuard` (tanpa role guard).
    app.get("/admin-invoices")
        .token(&support)
        .send()
        .await
        .assert_status(200)
        .assert_no_sql_error();
    let res = app
        .get("/admin-invoices?status=X&from=2026-03-01&to=2026-02-01&bar=1")
        .token(&support)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "status must be one of the following values: UNPAID, PAID, CANCELLED, EXPIRED",
            "from must not be later than to"
        ])
    );
}

#[tokio::test]
async fn daftar_filter_dan_bentuk() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let (op, token) = operator(&app, "FINANCE").await;
    let fx = fixture(&app, op, None).await;

    let res = app
        .get(&format!(
            "/admin-invoices?companyUuid={}&itemsPerPage=10",
            fx.company_uuid
        ))
        .token(&token)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(res.json["success"], true);
    assert_eq!(res.json["message"], "Success get all data invoices");
    let data = &res.json["data"];
    assert_eq!(data["total"], 4);
    assert_eq!(data["itemsPerPage"], 10);
    assert_eq!(data["pageNumber"], 1);
    let rows = data["result"].as_array().unwrap();
    // createdAt desc.
    let nums: Vec<&str> = rows
        .iter()
        .map(|r| r["invoiceNumber"].as_str().unwrap())
        .collect();
    assert_eq!(
        nums,
        [
            &format!("INV-{}-D", fx.tag),
            &format!("INV-{}-C", fx.tag),
            &format!("INV-{}-B", fx.tag),
            &format!("INV-{}-A", fx.tag)
        ]
        .map(String::as_str)
    );
    let a = &rows[3];
    let keys: Vec<&str> = a.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "id",
            "uuid",
            "subscriptionId",
            "invoiceNumber",
            "paymentUrl",
            "subPrice",
            "taxPrice",
            "feePrice",
            "totalPrice",
            "status",
            "invoiceType",
            "paymentProvider",
            "paypalPaymentId",
            "createdAt",
            "updatedAt",
            "expiredAt",
            "invoicePathPdf",
            "currency",
            "paymentMethod",
            "paymentReference",
            "paidAt",
            "periodStart",
            "periodEnd",
            "note",
            "prorationDeactivatedUserCompanyIds",
            "subscription",
            "recordedBy"
        ]
    );
    assert_eq!(a["totalPrice"], 150.5);
    assert_eq!(a["createdAt"], "2026-11-02T10:00:00.000Z");
    assert_eq!(a["prorationDeactivatedUserCompanyIds"], json!([]));
    assert_eq!(a["recordedBy"]["name"], "Finance Op");
    assert!(rows[0]["recordedBy"].is_null());
    let sub = &a["subscription"];
    assert_eq!(sub["companyId"], fx.company_id);
    assert_eq!(sub["startDate"], "2026-11-01T00:00:00.000Z");
    let sub_keys: Vec<&str> = sub
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(sub_keys.last(), Some(&"company"));
    let company = sub["company"].as_object().unwrap();
    assert_eq!(company["uuid"], fx.company_uuid.as_str());
    assert!(!company.contains_key("profilePictureFullPath"));

    // Filter: status, provider, search (tak peka huruf), rentang tanggal (to tanpa jam = s.d. akhir hari).
    let get = |q: String| {
        let token = token.clone();
        let app = &app;
        async move {
            let res = app
                .get(&format!("/admin-invoices?{q}"))
                .token(&token)
                .send()
                .await;
            res.assert_status(200).assert_no_sql_error();
            res.json["data"]["result"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["invoiceNumber"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        }
    };
    let tag = fx.tag.to_lowercase();
    assert_eq!(
        get(format!("search={tag}&status=PAID&provider=manual")).await,
        [format!("INV-{}-A", fx.tag)]
    );
    assert_eq!(
        get(format!("search={tag}&from=2026-11-03&to=2026-11-03")).await,
        [format!("INV-{}-B", fx.tag)]
    );
    assert_eq!(
        get(format!(
            "search={tag}&from=2026-11-02T10:00:00Z&to=2026-11-04T00:00:00Z"
        ))
        .await,
        [
            format!("INV-{}-C", fx.tag),
            format!("INV-{}-B", fx.tag),
            format!("INV-{}-A", fx.tag)
        ]
    );
    assert_eq!(
        get(format!("search={tag}&pageNumber=2&itemsPerPage=3")).await,
        [format!("INV-{}-A", fx.tag)]
    );
}

#[tokio::test]
async fn detail_dan_void() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let (op, finance) = operator(&app, "FINANCE").await;
    let fx = fixture(&app, op, None).await;

    let res = app
        .get(&format!("/admin-invoices/{}", fx.manual_paid))
        .token(&finance)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(res.json["message"], "Success");
    assert_eq!(res.json["data"]["recordedByOperatorId"], op);
    assert!(res.json["data"].get("subscription").is_none());
    for bad in [uuid::Uuid::new_v4().to_string(), "bukan-uuid".to_owned()] {
        let res = app
            .get(&format!("/admin-invoices/{bad}"))
            .token(&finance)
            .send()
            .await;
        res.assert_status(500);
        assert_eq!(
            res.json,
            json!({ "statusCode": 500, "message": "Internal server error" })
        );
    }

    let void = |uuid: &str, body: Value| {
        let app = &app;
        let finance = finance.clone();
        let uri = format!("/admin-invoices/{uuid}/void");
        async move { app.post(&uri).token(&finance).json(&body).send().await }
    };
    let res = void(&fx.manual_paid, json!({ "reason": "ab" })).await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!(["reason must be longer than or equal to 5 characters"])
    );
    let res = void("bukan-uuid", json!({ "reason": "Salah input" })).await;
    res.assert_status(400);
    assert_eq!(res.json["message"], "Validation failed (uuid is expected)");
    let res = void(
        &uuid::Uuid::new_v4().to_string(),
        json!({ "reason": "Salah input" }),
    )
    .await;
    res.assert_status(404);
    assert_eq!(
        res.json,
        json!({ "code": "INVOICE_NOT_FOUND", "message": "Invoice not found" })
    );
    let res = void(&fx.paypal, json!({ "reason": "Salah input" })).await;
    res.assert_status(409);
    assert_eq!(
        res.json,
        json!({ "code": "NOT_MANUAL_INVOICE", "message": "Only manual invoices can be voided." })
    );
    let res = void(&fx.cancelled, json!({ "reason": "Salah input" })).await;
    res.assert_status(409);
    assert_eq!(res.json["code"], "NOT_PAID");

    let res = void(&fx.manual_unpaid, json!({ "reason": "  Salah input  " })).await;
    res.assert_status(201).assert_no_sql_error();
    assert_eq!(
        res.json,
        json!({ "success": true, "message": "OK", "data": { "uuid": fx.manual_unpaid, "status": "CANCELLED" } })
    );
    let status: String = app
        .scalar(&format!(
            "SELECT status::text FROM invoices WHERE uuid = '{}'",
            fx.manual_unpaid
        ))
        .await;
    assert_eq!(status, "CANCELLED");
    let (before, after, reason, company): (Value, Value, String, i32) = sqlx::query_as(
        r#"SELECT before, after, reason, "companyId" FROM "operatorAuditLog"
            WHERE action = 'invoice.void' AND "targetUuid" = $1::uuid"#,
    )
    .bind(&fx.manual_unpaid)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(before["status"], "UNPAID");
    assert_eq!(before["totalPrice"], 150.5);
    assert_eq!(after, json!({ "status": "CANCELLED" }));
    assert_eq!(reason, "Salah input");
    assert_eq!(company, fx.company_id);
}

#[tokio::test]
async fn kirim_ulang_email() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let (op, finance) = operator(&app, "OWNER").await;
    let fx_company = app.company().await;
    let none = fixture(&app, op, None).await;
    // Company tanpa owner dan tanpa anggota default → 400.
    let res = app
        .post(&format!(
            "/admin-invoices/{}/resend-email",
            none.manual_paid
        ))
        .token(&finance)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json,
        json!({ "code": "NO_RECIPIENT", "message": "Company has no owner or default member to e-mail." })
    );
    let res = app
        .post(&format!(
            "/admin-invoices/{}/resend-email",
            uuid::Uuid::new_v4()
        ))
        .token(&finance)
        .send()
        .await;
    res.assert_status(404);

    let fx = fixture(&app, op, Some(fx_company.admin.id)).await;
    let uri = format!("/admin-invoices/{}/resend-email", fx.manual_paid);
    // Tanpa MAILTRAP_TOKEN pengiriman gagal sesudah commit → 502, baris audit (throttle) tetap.
    let res = app
        .post(&uri)
        .token(&finance)
        .json(&json!({ "reason": "Diminta klien" }))
        .send()
        .await;
    res.assert_status(502);
    assert_eq!(
        res.json,
        json!({ "code": "EMAIL_SEND_FAILED", "message": "Failed to send the invoice e-mail." })
    );
    let (after, reason, company): (Value, Option<String>, i32) = sqlx::query_as(
        r#"SELECT after, reason, "companyId" FROM "operatorAuditLog"
            WHERE action = 'invoice.resend_email' AND "targetUuid" = $1::uuid"#,
    )
    .bind(&fx.manual_paid)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(after["invoiceNumber"], format!("INV-{}-A", fx.tag));
    let owner_email: String = app
        .scalar(&format!(
            "SELECT username FROM users WHERE id = {}",
            fx_company.admin.id
        ))
        .await;
    assert_eq!(after["recipient"], owner_email.as_str());
    assert_eq!(reason.as_deref(), Some("Diminta klien"));
    assert_eq!(company, fx.company_id);

    // Dalam 5 menit → 429 dengan retryAfterSeconds, tanpa baris audit baru.
    let res = app.post(&uri).token(&finance).send().await;
    res.assert_status(429);
    assert_eq!(res.json["code"], "RESEND_TOO_SOON");
    assert_eq!(
        res.json["message"],
        "Invoice e-mail was sent recently. Try again later."
    );
    let retry = res.json["retryAfterSeconds"].as_i64().unwrap();
    assert!((1..=300).contains(&retry), "{retry}");
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "operatorAuditLog" WHERE action = 'invoice.resend_email' AND "targetUuid" = '{}'"#,
            fx.manual_paid
        ))
        .await;
    assert_eq!(n, 1);
    // Status / nominal invoice tidak berubah.
    let (status, total): (String, f64) =
        sqlx::query_as(r#"SELECT status::text, "totalPrice" FROM invoices WHERE uuid = $1::uuid"#)
            .bind(&fx.manual_paid)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!((status.as_str(), total), ("PAID", 150.5));
    let res = app
        .post(&uri)
        .token(&finance)
        .json(&json!({ "reason": "ab" }))
        .send()
        .await;
    res.assert_status(400);
}
