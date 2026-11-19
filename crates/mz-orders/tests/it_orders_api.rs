//! Uji integrasi `mz-orders` terhadap Postgres nyata (template `mudah_test_template`).
//! Modul order tidak menyentuh GCS/layanan luar, jadi tanpa emulator.
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-orders --test it_orders_api`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    dead_code
)]

use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use mz_core::AppState;
use mz_testkit::{Company, TestApp, TestResponse};
use serde_json::{Value, json};

/// Test yang mengubah baris `company` (batas diskon) memegang kunci ini.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn build(state: &AppState) -> Router {
    mz_orders::router(state)
        .merge(mz_table_fields::router(state))
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

fn tag() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{:X}{:X}",
        (nanos as u64) & 0xFF_FFFF,
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
    .chars()
    .take(10)
    .collect()
}

async fn setup() -> Option<(TestApp, Company)> {
    let app = TestApp::new(build).await?;
    let c = app.company().await;
    app.exec(
        r#"INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
             SELECT al.id, k.f, a.a::"PermissionAction" FROM "accessLevel" al
             CROSS JOIN (VALUES ('orders'), ('order-masters'), ('order-approvals'), ('order-deliveries'),
                                ('products'), ('customers'), ('stores')) AS k(f)
             CROSS JOIN (VALUES ('READ'), ('WRITE'), ('DELETE')) AS a(a)
             WHERE al."isSystem" ON CONFLICT DO NOTHING;"#,
    )
    .await;
    Some((app, c))
}

/// Fixture katalog + akun + master order.
struct Fx {
    customer: String,
    store: String,
    store_id: i32,
    product: String,
    pcs: String,
    boxu: String,
    sku: String,
    type_auto: String,
    type_appr: String,
    reason_cancel: String,
    reason_delivery: String,
    reason_no_order: String,
    today: String,
    prefix: String,
}

async fn master(app: &TestApp, c: &Company, kind: &str, body: Value) -> String {
    let r = app
        .post(&format!("/v1/admin/order-masters/{kind}"))
        .token(&c.admin.token)
        .json(&body)
        .send()
        .await;
    r.assert_status(201);
    r.data()["id"].as_str().unwrap().to_owned()
}

async fn fixture(app: &TestApp, c: &Company) -> Fx {
    let t = tag();
    let (cid, uid) = (c.id, c.admin.id);
    let pl: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "priceList" ("companyId", code, name, "currencyCode", "createdById")
               VALUES ({cid}, 'PL{t}', 'PL {t}', 'IDR', {uid}) RETURNING id"#
        ))
        .await;
    let (customer_id, customer): (i32, String) = sqlx::query_as(&format!(
        r#"INSERT INTO customer ("companyId", code, name, "createdById", "priceListId")
           VALUES ({cid}, 'CU{t}', 'Cust {t}', {uid}, {pl}) RETURNING id, uuid::text"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    let (store_id, store): (i32, String) = sqlx::query_as(&format!(
        r#"INSERT INTO store ("companyId", code, name, "ownerName", address, lat, lng, "createdById", "customerId")
           VALUES ({cid}, 'ST{t}', 'Toko {t}', 'Owner', 'Jl. Uji', -6.2, 106.8, {uid}, {customer_id}) RETURNING id, uuid::text"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    let cat: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "productCategory" ("companyId", code, name, "createdById")
               VALUES ({cid}, 'C{t}', 'Cat {t}', {uid}) RETURNING id"#
        ))
        .await;
    let tax: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "taxRate" ("companyId", code, name, rate, "createdById")
               VALUES ({cid}, 'T{t}', 'PPN {t}', 11, {uid}) RETURNING id"#
        ))
        .await;
    let pcs_id: i32 = app
        .scalar(&format!(
            r#"SELECT id FROM uom WHERE "companyId" = {cid} AND code = 'PCS'"#
        ))
        .await;
    let box_id: i32 = app
        .scalar(&format!(
            r#"SELECT id FROM uom WHERE "companyId" = {cid} AND code = 'BOX'"#
        ))
        .await;
    let sku = format!("SKU-{t}");
    let (pid, product): (i32, String) = sqlx::query_as(&format!(
        r#"INSERT INTO product ("companyId", sku, name, "categoryId", "baseUomId", "taxRateId", "createdById", "customFields")
           VALUES ({cid}, '{sku}', 'Produk {t}', {cat}, {pcs_id}, {tax}, {uid}, '{{}}') RETURNING id, uuid::text"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    app.exec(&format!(
        r#"INSERT INTO "productUom" ("productId", "uomId", "conversionRate", operator, factor, "isBase")
             VALUES ({pid}, {pcs_id}, 1, 'multiply', 1, true), ({pid}, {box_id}, 24, 'multiply', 24, false);
           INSERT INTO "priceListItem" ("companyId", "priceListId", "productId", "uomId", "minQty", price)
             VALUES ({cid}, {pl}, {pid}, {pcs_id}, 1, 1000);"#
    ))
    .await;
    let pcs: String = app
        .scalar(&format!("SELECT uuid::text FROM uom WHERE id = {pcs_id}"))
        .await;
    let boxu: String = app
        .scalar(&format!("SELECT uuid::text FROM uom WHERE id = {box_id}"))
        .await;
    let prefix = format!("Q{}", t.get(..7).unwrap_or(&t));
    let type_auto = master(
        app,
        c,
        "types",
        json!({"code": format!("A{t}"), "name": "Auto"}),
    )
    .await;
    let type_appr = master(
        app,
        c,
        "types",
        json!({"code": format!("P{t}"), "name": "Appr", "requiresApproval": true, "numberPrefix": prefix.to_lowercase()}),
    )
    .await;
    let reason_cancel = master(
        app,
        c,
        "reasons",
        json!({"code": format!("X{t}"), "name": "Batal", "reasonType": "order_cancel"}),
    )
    .await;
    let reason_delivery = master(
        app,
        c,
        "reasons",
        json!({"code": format!("D{t}"), "name": "Rusak", "reasonType": "delivery_adjustment"}),
    )
    .await;
    let reason_no_order = master(
        app,
        c,
        "reasons",
        json!({"code": format!("N{t}"), "name": "Tutup", "reasonType": "no_order"}),
    )
    .await;
    let today: String = app
        .scalar(&format!(
            r#"SELECT to_char((now() AT TIME ZONE "timeZone")::date, 'YYYY-MM-DD') FROM company WHERE id = {cid}"#
        ))
        .await;
    Fx {
        customer,
        store,
        store_id,
        product,
        pcs,
        boxu,
        sku,
        type_auto,
        type_appr,
        reason_cancel,
        reason_delivery,
        reason_no_order,
        today,
        prefix,
    }
}

fn uuid4() -> String {
    uuid::Uuid::new_v4().to_string()
}

async fn create_draft(app: &TestApp, c: &Company, fx: &Fx, order_type: &str) -> String {
    let r = app
        .post("/v1/admin/orders")
        .token(&c.admin.token)
        .json(&json!({
            "clientRequestId": uuid4(), "orderTypeId": order_type, "storeId": fx.store,
            "orderDate": fx.today, "note": "catatan", "customFields": {},
        }))
        .send()
        .await;
    r.assert_status(201);
    r.data()["id"].as_str().unwrap().to_owned()
}

async fn put_items(app: &TestApp, c: &Company, id: &str, items: Value) -> TestResponse {
    app.put(&format!("/v1/admin/orders/{id}/items"))
        .token(&c.admin.token)
        .json(&json!({ "items": items }))
        .send()
        .await
}

#[tokio::test]
async fn masters_crud_and_rejections() {
    let Some((app, c)) = setup().await else {
        return;
    };
    let t = tag();
    let r = app
        .post("/v1/admin/order-masters/payment-terms")
        .token(&c.admin.token)
        .json(&json!({"code": format!("net{t}"), "name": "Net 30", "dueDays": "30", "isDefault": true}))
        .send()
        .await;
    r.assert_status(201);
    let d = r.data().clone();
    assert_eq!(d["code"], format!("NET{t}"));
    assert_eq!(d["dueDays"], 30);
    assert_eq!(d["isDefault"], true);
    let keys: Vec<&String> = d.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "id",
            "code",
            "name",
            "isActive",
            "isDefault",
            "dueDays",
            "createdAt",
            "updatedAt"
        ]
    );
    let id = d["id"].as_str().unwrap();

    // Kode sama → 409; field kind lain → 422; reasonType wajib untuk reasons.
    app.post("/v1/admin/order-masters/payment-terms")
        .token(&c.admin.token)
        .json(&json!({"code": format!("NET{t}"), "name": "dup"}))
        .send()
        .await
        .assert_status(409);
    let r = app
        .post("/v1/admin/order-masters/types")
        .token(&c.admin.token)
        .json(&json!({"code": format!("Z{t}"), "name": "x", "dueDays": 3}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(r.json["code"], "ORDER_MASTER_INVALID_FIELD");
    assert_eq!(r.json["message"], "dueDays is not accepted for Order type");
    let r = app
        .post("/v1/admin/order-masters/reasons")
        .token(&c.admin.token)
        .json(&json!({"code": format!("R{t}"), "name": "x"}))
        .send()
        .await;
    assert_eq!(r.json["code"], "ORDER_REASON_TYPE_REQUIRED");
    app.get("/v1/admin/order-masters/bogus")
        .token(&c.admin.token)
        .send()
        .await
        .assert_status(400);
    let r = app
        .post("/v1/admin/order-masters/types")
        .token(&c.admin.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "code must be 1-32 characters of uppercase letters, digits, or underscore",
            "code must be a string",
            "name must be longer than or equal to 1 characters",
            "name must be a string"
        ])
    );

    let r = app
        .get("/v1/admin/order-masters/payment-terms?isDefault=true&search=net")
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert!(r.data().as_array().unwrap().iter().any(|x| x["id"] == id));
    app.get("/v1/admin/order-masters/reasons?isDefault=true")
        .token(&c.admin.token)
        .send()
        .await
        .assert_status(422);
    app.get(&format!("/v1/admin/order-masters/payment-terms/{id}"))
        .token(&c.admin.token)
        .send()
        .await
        .assert_status(200);
    let r = app
        .patch(&format!("/v1/admin/order-masters/payment-terms/{id}"))
        .token(&c.admin.token)
        .json(&json!({"name": "Net 45", "dueDays": 45}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["dueDays"], 45);
    let r = app
        .delete(&format!("/v1/admin/order-masters/payment-terms/{id}"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data(), &json!({"id": id, "archived": true}));
    app.get(&format!("/v1/admin/order-masters/types/{}", uuid4()))
        .token(&c.admin.token)
        .send()
        .await
        .assert_status(404);
    app.get("/v1/admin/order-masters/types")
        .token(&c.staff.token)
        .send()
        .await
        .assert_status(401);
    app.get("/v1/admin/order-masters/types")
        .send()
        .await
        .assert_status(401);
    let audits: i64 = app
        .scalar(&format!(r#"SELECT COUNT(*) FROM "auditLog" WHERE "entityType" = 'orderPaymentTerm' AND "companyId" = {}"#, c.id))
        .await;
    assert!(audits >= 1);
}

#[tokio::test]
async fn custom_fields_crud() {
    let Some((app, c)) = setup().await else {
        return;
    };
    let key = format!("po_{}", tag().to_lowercase());
    let epoch_before: i32 = app
        .scalar(&format!(
            r#"SELECT "catalogEpoch" FROM company WHERE id = {}"#,
            c.id
        ))
        .await;
    let r = app
        .post("/v1/admin/order-custom-fields")
        .token(&c.admin.token)
        .json(
            &json!({"entity": "order", "key": key, "label": "PO", "type": "select",
                      "options": [{"value": "a", "label": "A"}]}),
        )
        .send()
        .await;
    r.assert_status(201);
    let id = r.data()["id"].as_str().unwrap().to_owned();
    assert_eq!(r.data()["entity"], "order");
    let epoch_after: i32 = app
        .scalar(&format!(
            r#"SELECT "catalogEpoch" FROM company WHERE id = {}"#,
            c.id
        ))
        .await;
    assert!(epoch_after > epoch_before);
    let r = app
        .post("/v1/admin/order-custom-fields")
        .token(&c.admin.token)
        .json(&json!({"entity": "order", "key": key, "label": "PO", "type": "text"}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "CUSTOM_FIELD_KEY_TAKEN");
    app.get("/v1/admin/order-custom-fields")
        .token(&c.admin.token)
        .send()
        .await
        .assert_status(400);
    let r = app
        .get("/v1/admin/order-custom-fields?entity=order&search=po_")
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert!(r.json["meta"]["total"].as_i64().unwrap() >= 1);
    let r = app
        .patch(&format!("/v1/admin/order-custom-fields/{id}"))
        .token(&c.admin.token)
        .json(&json!({"type": "text"}))
        .send()
        .await;
    assert_eq!(r.json["code"], "CUSTOM_FIELD_IMMUTABLE");
    let r = app
        .patch(&format!("/v1/admin/order-custom-fields/{id}"))
        .token(&c.admin.token)
        .json(&json!({"options": [{"value": "b", "label": "B"}]}))
        .send()
        .await;
    assert_eq!(r.json["code"], "CUSTOM_FIELD_OPTIONS_IMMUTABLE");
    let r = app
        .patch(&format!("/v1/admin/order-custom-fields/{id}"))
        .token(&c.admin.token)
        .json(&json!({"label": "PO Number", "options": [{"value": "a", "label": "A"}, {"value": "b", "label": "B"}]}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["options"].as_array().unwrap().len(), 2);
    let r = app
        .delete(&format!("/v1/admin/order-custom-fields/{id}"))
        .token(&c.admin.token)
        .send()
        .await;
    assert_eq!(r.data(), &json!({"id": id, "archived": true}));
    app.get(&format!("/v1/admin/order-custom-fields/{}", uuid4()))
        .token(&c.admin.token)
        .send()
        .await
        .assert_status(404);
}

#[tokio::test]
async fn admin_order_lifecycle() {
    let Some((app, c)) = setup().await else {
        return;
    };
    let _g = SERIAL.lock().await;
    app.exec(&format!(
        r#"UPDATE company SET "maxOrderDiscountPercent" = NULL WHERE id = {}"#,
        c.id
    ))
    .await;
    let fx = fixture(&app, &c).await;

    // Validasi & jangkar.
    let r = app
        .post("/v1/admin/orders")
        .token(&c.admin.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    let r = app
        .post("/v1/admin/orders")
        .token(&c.admin.token)
        .json(&json!({"clientRequestId": uuid4(), "orderTypeId": fx.type_auto, "orderDate": fx.today}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(r.json["code"], "CUSTOMER_OR_STORE_REQUIRED");
    let r = app
        .post("/v1/admin/orders")
        .token(&c.admin.token)
        .json(&json!({"clientRequestId": uuid4(), "orderTypeId": fx.type_auto, "storeId": fx.store, "orderDate": "2020-01-01"}))
        .send()
        .await;
    assert_eq!(r.json["code"], "ORDER_DATE_OUT_OF_RANGE");

    // Create draft (header).
    let crid = uuid4();
    let r = app
        .post("/v1/admin/orders")
        .token(&c.admin.token)
        .json(&json!({"clientRequestId": crid, "orderTypeId": fx.type_auto, "storeId": fx.store, "orderDate": fx.today}))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    let d = r.data().clone();
    let id = d["id"].as_str().unwrap().to_owned();
    assert_eq!(d["status"], "draft");
    assert!(d["orderNumber"].as_str().unwrap().starts_with("DRAFT-"));
    assert_eq!(d["currencyCode"], "IDR");
    assert!(d["customer"]["code"].as_str().unwrap().len() > 2);
    assert!(d.get("items").is_none());
    let keys: Vec<&String> = d.as_object().unwrap().keys().collect();
    assert_eq!(
        keys[..6],
        [
            "id",
            "orderNumber",
            "status",
            "source",
            "orderType",
            "customer"
        ]
    );
    assert_eq!(*keys.last().unwrap(), "updatedAt");
    // clientRequestId sama → 409 + details.
    let r = app
        .post("/v1/admin/orders")
        .token(&c.admin.token)
        .json(&json!({"clientRequestId": crid, "orderTypeId": fx.type_auto, "storeId": fx.store, "orderDate": fx.today}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "ORDER_CLIENT_REQUEST_CONFLICT");
    assert_eq!(r.json["details"]["orderId"], id.as_str());

    // REPLACE items: 2 BOX (harga turunan PCS × 24) + 3 PCS diskon 10%.
    let r = put_items(
        &app,
        &c,
        &id,
        json!([
            {"productId": fx.product, "uomId": fx.boxu, "qty": "2"},
            {"productId": fx.product, "uomId": fx.pcs, "qty": "3", "discountPercent": "10"}
        ]),
    )
    .await;
    r.assert_status(200).assert_no_sql_error();
    let d = r.data().clone();
    assert_eq!(d["items"][0]["unitPrice"], "24000");
    assert_eq!(d["items"][0]["grossAmount"], "48000");
    assert_eq!(d["items"][0]["taxAmount"], "5280");
    assert_eq!(d["items"][0]["factor"], "24");
    assert_eq!(d["items"][1]["discountValue"], "300");
    assert_eq!(d["subtotal"], "51000");
    assert_eq!(d["discountTotal"], "300");
    assert_eq!(d["grandTotal"], "56277");
    assert_eq!(d["itemCount"], 2);
    assert_eq!(d["effectiveDiscountPercent"], "0.59");
    let r = put_items(
        &app,
        &c,
        &id,
        json!([{"productId": uuid4(), "uomId": fx.pcs, "qty": "1"}]),
    )
    .await;
    r.assert_status(404);
    assert_eq!(r.json["message"], "Product not found for item 1");
    let r = put_items(
        &app,
        &c,
        &id,
        json!([{"productId": fx.product, "uomId": fx.pcs, "qty": "0"}]),
    )
    .await;
    assert_eq!(r.json["code"], "ITEM_QTY_INVALID");

    // PATCH header.
    let r = app
        .patch(&format!("/v1/admin/orders/{id}"))
        .token(&c.admin.token)
        .json(&json!({"note": "baru", "requestedDeliveryDate": fx.today}))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.data()["note"], "baru");

    // Detail, list, owners, config.
    let r = app
        .get(&format!("/v1/admin/orders/{id}"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["items"].as_array().unwrap().len(), 2);
    assert_eq!(r.data()["deliveryHistory"], json!([]));
    let r = app
        .get(&format!(
            "/v1/admin/orders?status=draft&storeId={}&q={}&sortBy=grandTotal&sortDir=asc",
            fx.store, fx.sku
        ))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let r = app
        .get(&format!(
            "/v1/admin/orders?status=draft&storeId={}&cf%5Bpo%5D=x&dateFrom={}",
            fx.store, fx.today
        ))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["meta"]["total"], 0);
    let r = app
        .get(&format!("/v1/admin/orders?storeId={}", fx.store))
        .token(&c.admin.token)
        .send()
        .await;
    assert_eq!(r.json["meta"]["total"], 1);
    let r = app
        .get("/v1/admin/orders/owners?search=a&limit=5")
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let r = app
        .get("/v1/admin/orders/config")
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert!(r.data()["timeZone"].is_string());
    assert!(
        r.data()["orderReasons"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["reasonType"] != "delivery_adjustment")
    );

    // Invoice belum bisa (draft).
    let r = app
        .get(&format!("/v1/admin/orders/{id}/invoice-pdf"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "ORDER_NOT_INVOICEABLE");

    // Submit → approved, nomor `<prefix>-YYYYMM-NNNNN`.
    let r = app
        .post(&format!("/v1/admin/orders/{id}/submit"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let d = r.data().clone();
    assert_eq!(d["status"], "approved");
    let number = d["orderNumber"].as_str().unwrap().to_owned();
    assert!(
        number.starts_with(&format!(
            "ORD-{}-",
            fx.today.replace('-', "").get(..6).unwrap()
        )),
        "{number}"
    );
    assert!(d["approvedAt"].is_number());
    assert!(d["approvedBy"].is_null());
    // Bukan draft lagi → PATCH 409, submit 409.
    app.patch(&format!("/v1/admin/orders/{id}"))
        .token(&c.admin.token)
        .json(&json!({"note": "x"}))
        .send()
        .await
        .assert_status(409);
    let r = app
        .post(&format!("/v1/admin/orders/{id}/submit"))
        .token(&c.admin.token)
        .send()
        .await;
    assert_eq!(r.json["code"], "ORDER_STATE_CONFLICT");

    // Invoice PDF.
    let r = app
        .get(&format!("/v1/admin/orders/{id}/invoice-pdf"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.headers["content-type"], "application/pdf");
    assert_eq!(
        r.headers["content-disposition"].to_str().unwrap(),
        format!("attachment; filename=\"invoice-{number}.pdf\"")
    );
    assert!(r.bytes.starts_with(b"%PDF-"));

    // Export xlsx/csv, level order & item.
    let r = app
        .get(&format!("/v1/admin/orders/export?storeId={}", fx.store))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.headers["content-type"],
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    );
    assert!(r.bytes.starts_with(b"PK"));
    let r = app
        .get(&format!(
            "/v1/admin/orders/export?format=csv&storeId={}",
            fx.store
        ))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(200);
    let text = r.text();
    assert!(
        text.starts_with("\u{feff}id,orderNumber,status,source"),
        "{text}"
    );
    assert!(text.contains(&number));
    let r = app
        .get(&format!(
            "/v1/admin/orders/export?format=csv&level=item&storeId={}",
            fx.store
        ))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.text().lines().count(), 3);
    assert_eq!(
        r.headers["content-disposition"].to_str().unwrap(),
        "attachment; filename=\"order-lines-export.csv\""
    );

    // Cancel tanpa alasan → 422; dengan alasan tipe salah → 422.
    let dup = app
        .post(&format!("/v1/admin/orders/{id}/duplicate"))
        .token(&c.admin.token)
        .send()
        .await;
    dup.assert_status(201).assert_no_sql_error();
    let dup_id = dup.data()["id"].as_str().unwrap().to_owned();
    assert_eq!(dup.data()["status"], "draft");
    assert_eq!(dup.data()["items"].as_array().unwrap().len(), 2);
    let r = app
        .post(&format!("/v1/admin/orders/{dup_id}/cancel"))
        .token(&c.admin.token)
        .json(&json!({}))
        .send()
        .await;
    assert_eq!(r.json["code"], "CANCEL_REASON_REQUIRED");
    let r = app
        .post(&format!("/v1/admin/orders/{dup_id}/cancel"))
        .token(&c.admin.token)
        .json(&json!({"reasonId": fx.reason_no_order}))
        .send()
        .await;
    assert_eq!(r.json["code"], "REASON_TYPE_MISMATCH");
    let r = app
        .post(&format!("/v1/admin/orders/{dup_id}/cancel"))
        .token(&c.admin.token)
        .json(&json!({"reasonId": fx.reason_cancel, "note": "salah input"}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["status"], "cancelled");
    assert_eq!(r.data()["cancelNote"], "salah input");

    // Audit `order` tercatat.
    let audits: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "auditLog" WHERE "entityType" = 'order' AND "companyId" = {}"#,
            c.id
        ))
        .await;
    assert!(audits >= 3);
    app.get(&format!("/v1/admin/orders/{}", uuid4()))
        .token(&c.admin.token)
        .send()
        .await
        .assert_status(404);
    app.get("/v1/admin/orders")
        .token(&c.staff.token)
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
async fn approval_flow() {
    let Some((app, c)) = setup().await else {
        return;
    };
    let _g = SERIAL.lock().await;
    app.exec(&format!(
        r#"UPDATE company SET "maxOrderDiscountPercent" = 5 WHERE id = {}"#,
        c.id
    ))
    .await;
    let fx = fixture(&app, &c).await;
    let id = create_draft(&app, &c, &fx, &fx.type_appr).await;
    // Kosong → ORDER_EMPTY (nomor tidak terpakai karena rollback).
    let r = app
        .post(&format!("/v1/admin/orders/{id}/submit"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(r.json["code"], "ORDER_EMPTY");
    // Diskon 10% > batas 5% → DISCOUNT_LIMIT_EXCEEDED.
    put_items(
        &app,
        &c,
        &id,
        json!([{"productId": fx.product, "uomId": fx.pcs, "qty": "10", "discountPercent": "10"}]),
    )
    .await
    .assert_status(200);
    let r = app
        .post(&format!("/v1/admin/orders/{id}/submit"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(r.json["code"], "DISCOUNT_LIMIT_EXCEEDED");
    assert_eq!(
        r.json["details"],
        json!({"percentUsed": "10", "maxDiscountPercent": "5"})
    );
    put_items(
        &app,
        &c,
        &id,
        json!([{"productId": fx.product, "uomId": fx.pcs, "qty": "10"}]),
    )
    .await
    .assert_status(200);
    let r = app
        .post(&format!("/v1/admin/orders/{id}/submit"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["status"], "pending_approval");
    let number = r.data()["orderNumber"].as_str().unwrap().to_owned();
    assert!(
        number.starts_with(&format!("{}-", fx.prefix)) && number.ends_with("-00001"),
        "{number}"
    );
    let r = app
        .get("/v1/admin/order-approvals")
        .token(&c.manager.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert!(r.data().as_array().unwrap().iter().any(|o| o["id"] == id));
    assert!(r.json.get("fieldPolicy").is_none());
    let r = app
        .post(&format!("/v1/admin/order-approvals/{id}/reject"))
        .token(&c.manager.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    let r = app
        .post(&format!("/v1/admin/order-approvals/{id}/approve"))
        .token(&c.manager.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["status"], "approved");
    assert_eq!(r.data()["approvedBy"]["id"], c.manager.uuid.as_str());
    app.post(&format!("/v1/admin/order-approvals/{id}/approve"))
        .token(&c.manager.token)
        .send()
        .await
        .assert_status(409);
    // Order kedua: reject.
    let id2 = create_draft(&app, &c, &fx, &fx.type_appr).await;
    put_items(
        &app,
        &c,
        &id2,
        json!([{"productId": fx.product, "uomId": fx.pcs, "qty": "1"}]),
    )
    .await
    .assert_status(200);
    let r = app
        .post(&format!("/v1/admin/orders/{id2}/submit"))
        .token(&c.admin.token)
        .send()
        .await;
    assert!(
        r.data()["orderNumber"]
            .as_str()
            .unwrap()
            .ends_with("-00002")
    );
    let r = app
        .post(&format!("/v1/admin/order-approvals/{id2}/reject"))
        .token(&c.manager.token)
        .json(&json!({"note": "harga salah"}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["status"], "rejected");
    assert_eq!(r.data()["rejectNote"], "harga salah");
    app.exec(&format!(
        r#"UPDATE company SET "maxOrderDiscountPercent" = NULL WHERE id = {}"#,
        c.id
    ))
    .await;
}

#[tokio::test]
async fn delivery_flow() {
    let Some((app, c)) = setup().await else {
        return;
    };
    let fx = fixture(&app, &c).await;
    let id = create_draft(&app, &c, &fx, &fx.type_auto).await;
    let r = put_items(
        &app,
        &c,
        &id,
        json!([
            {"productId": fx.product, "uomId": fx.pcs, "qty": "5"},
            {"productId": fx.product, "uomId": fx.boxu, "qty": "1"}
        ]),
    )
    .await;
    let item1 = r.data()["items"][0]["id"].as_str().unwrap().to_owned();
    // Belum approved → 409.
    let deliver = |body: Value| {
        let app = &app;
        let id = id.clone();
        let token = c.admin.token.clone();
        async move {
            app.post(&format!("/v1/admin/order-deliveries/{id}/deliver"))
                .token(&token)
                .json(&body)
                .send()
                .await
        }
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    let crid = uuid4();
    deliver(json!({"clientRequestId": crid, "deliveredAt": now_ms}))
        .await
        .assert_status(409);
    app.post(&format!("/v1/admin/orders/{id}/submit"))
        .token(&c.admin.token)
        .send()
        .await
        .assert_status(200);
    let now_ms = chrono::Utc::now().timestamp_millis();
    app.post("/v1/admin/order-deliveries/not-a-uuid/deliver")
        .token(&c.admin.token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(400);
    let r = app
        .get("/v1/admin/order-deliveries")
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert!(r.data().as_array().unwrap().iter().any(|o| o["id"] == id));
    // Kurang tanpa alasan → 422 DELIVERY_REASON_REQUIRED.
    let r = deliver(json!({"clientRequestId": crid, "deliveredAt": now_ms,
                           "lines": [{"itemId": item1, "deliveredQty": "3"}]}))
    .await;
    r.assert_status(422);
    assert_eq!(r.json["code"], "DELIVERY_REASON_REQUIRED");
    let r = deliver(json!({"clientRequestId": crid, "deliveredAt": now_ms + 3_600_000})).await;
    assert_eq!(r.json["code"], "DELIVERED_AT_IN_FUTURE");
    let r = deliver(json!({"clientRequestId": crid, "deliveredAt": now_ms,
                           "lines": [{"itemId": item1, "deliveredQty": "3", "reasonId": fx.reason_delivery, "note": " rusak "}]}))
    .await;
    r.assert_status(200).assert_no_sql_error();
    let d = r.data().clone();
    assert_eq!(d["status"], "delivered");
    assert_eq!(d["deliveryRevision"], 1);
    assert_eq!(d["items"][0]["deliveredQty"], "3");
    assert_eq!(d["items"][0]["shortQty"], "2");
    assert_eq!(d["items"][0]["deliveryNote"], "rusak");
    assert_eq!(d["items"][1]["shortQty"], "0");
    assert_eq!(d["finalSubtotal"], "27000");
    let h = &d["deliveryHistory"];
    assert_eq!(h.as_array().unwrap().len(), 1);
    assert_eq!(h[0]["kind"], "initial");
    assert_eq!(h[0]["lines"].as_array().unwrap().len(), 1);
    assert_eq!(h[0]["lines"][0]["reason"]["name"], "Rusak");
    // Replay kunci sama → 200, tidak tercatat dua kali.
    let r = deliver(json!({"clientRequestId": crid, "deliveredAt": now_ms})).await;
    r.assert_status(200);
    assert_eq!(r.data()["deliveryHistory"].as_array().unwrap().len(), 1);
    deliver(json!({"clientRequestId": uuid4(), "deliveredAt": now_ms}))
        .await
        .assert_status(409);
    // Koreksi: revisi salah → 409; tanpa perubahan → 422; benar → revisi 2.
    let correct = |body: Value| {
        let app = &app;
        let id = id.clone();
        let token = c.admin.token.clone();
        async move {
            app.post(&format!("/v1/admin/order-deliveries/{id}/correct"))
                .token(&token)
                .json(&body)
                .send()
                .await
        }
    };
    let r = correct(json!({"expectedRevision": 5, "note": "fix"})).await;
    r.assert_status(409);
    assert_eq!(
        r.json["details"],
        json!({"expectedRevision": 5, "currentRevision": 1})
    );
    let r = correct(json!({"expectedRevision": 1, "note": "fix"})).await;
    assert_eq!(r.json["code"], "DELIVERY_NO_CHANGES");
    let r = correct(json!({"expectedRevision": 1, "note": "fix", "lines": [{"itemId": item1, "deliveredQty": "5"}]})).await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.data()["deliveryRevision"], 2);
    assert_eq!(r.data()["items"][0]["deliveredQty"], "5");
    assert_eq!(r.data()["items"][0]["deliveryReason"], Value::Null);
    assert_eq!(r.data()["deliveryHistory"][1]["kind"], "correction");
    // Order delivered tidak bisa dibatalkan.
    let r = app
        .post(&format!("/v1/admin/orders/{id}/cancel"))
        .token(&c.admin.token)
        .json(&json!({"reasonId": fx.reason_cancel}))
        .send()
        .await;
    r.assert_status(409);
    // Invoice delivered.
    app.get(&format!("/v1/admin/orders/{id}/invoice-pdf"))
        .token(&c.admin.token)
        .send()
        .await
        .assert_status(200);

    // Mobile delivery: config, list, detail.
    let r = app
        .get("/v1/mobile/order-deliveries/config")
        .token(&c.staff.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert!(
        r.data()["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"] == fx.reason_delivery.as_str())
    );
    let r = app
        .get("/v1/mobile/order-deliveries?status=approved&status=delivered&limit=200")
        .token(&c.staff.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let r = app
        .get(&format!("/v1/mobile/order-deliveries/{id}"))
        .token(&c.staff.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["id"], id.as_str());
    app.get(&format!("/v1/mobile/order-deliveries/{}", uuid4()))
        .token(&c.staff.token)
        .send()
        .await
        .assert_status(404);
    app.get("/v1/mobile/order-deliveries?cursor=%21%21")
        .token(&c.staff.token)
        .send()
        .await
        .assert_status(400);
    let _ = fx.store_id;
}

/// `POST /v1/mobile/order-deliveries/:id/deliver`: jalur mobile memakai `deliveries::deliver` yang
/// sama dengan admin, tetapi dengan `source = 'mobile'` dan filter scope region (R6).
#[tokio::test]
async fn mobile_delivery_deliver() {
    let Some((app, c)) = setup().await else {
        return;
    };
    // staff2 diberi assignment region sementara; `mobile_orders_and_no_order` juga memakai staff2.
    let _g = SERIAL.lock().await;
    let fx = fixture(&app, &c).await;
    let id = create_draft(&app, &c, &fx, &fx.type_auto).await;
    let r = put_items(
        &app,
        &c,
        &id,
        json!([{"productId": fx.product, "uomId": fx.pcs, "qty": "4"}]),
    )
    .await;
    r.assert_status(200);
    let item = r.data()["items"][0]["id"].as_str().unwrap().to_owned();
    let url = format!("/v1/mobile/order-deliveries/{id}/deliver");
    let now_ms = chrono::Utc::now().timestamp_millis();
    let crid = uuid4();
    let body = json!({"clientRequestId": crid, "deliveredAt": now_ms});

    // Gerbang + pipe.
    app.post(&url).json(&body).send().await.assert_status(401);
    app.post("/v1/mobile/order-deliveries/not-a-uuid/deliver")
        .token(&c.staff.token)
        .json(&body)
        .send()
        .await
        .assert_status(400);
    app.post(&format!("/v1/mobile/order-deliveries/{}/deliver", uuid4()))
        .token(&c.staff.token)
        .json(&body)
        .send()
        .await
        .assert_status(404);
    // Validasi DTO jalan sebelum service.
    app.post(&url)
        .token(&c.staff.token)
        .json(&json!({"deliveredAt": now_ms}))
        .send()
        .await
        .assert_status(400);
    // Masih draft → 409.
    let r = app
        .post(&url)
        .token(&c.staff.token)
        .json(&body)
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "ORDER_STATE_CONFLICT");
    app.post(&format!("/v1/admin/orders/{id}/submit"))
        .token(&c.admin.token)
        .send()
        .await
        .assert_status(200);
    // `deliveredAt` tidak boleh sebelum `approvedAt`.
    let now_ms = chrono::Utc::now().timestamp_millis();
    let body = json!({"clientRequestId": crid, "deliveredAt": now_ms});

    // Scope region: toko order masuk region A, staff2 hanya di region B → order "tidak ada".
    let (cid, uid) = (c.id, c.admin.id);
    let t = tag();
    let group_a: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "storeGroup" ("companyId", code, name, "createdById")
               VALUES ({cid}, 'GA{t}', 'Region A {t}', {uid}) RETURNING id"#
        ))
        .await;
    let group_b: i32 = app
        .scalar(&format!(
            r#"INSERT INTO "storeGroup" ("companyId", code, name, "createdById")
               VALUES ({cid}, 'GB{t}', 'Region B {t}', {uid}) RETURNING id"#
        ))
        .await;
    app.exec(&format!(
        r#"UPDATE store SET "groupId" = {group_a} WHERE id = {};
           INSERT INTO "storeGroupUser" ("groupId", "userId") VALUES ({group_b}, {});"#,
        fx.store_id, c.staff2.id
    ))
    .await;
    let r = app
        .post(&url)
        .token(&c.staff2.token)
        .json(&body)
        .send()
        .await;
    app.exec(&format!(
        r#"UPDATE "storeGroupUser" SET "deletedAt" = NOW() WHERE "groupId" = {group_b}"#
    ))
    .await;
    r.assert_status(404).assert_no_sql_error();

    // Kurang tanpa alasan → 422 (aturan sama dengan dashboard).
    let r = app
        .post(&url)
        .token(&c.staff.token)
        .json(&json!({"clientRequestId": crid, "deliveredAt": now_ms,
                      "lines": [{"itemId": item, "deliveredQty": "1"}]}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(r.json["code"], "DELIVERY_REASON_REQUIRED");

    // Sukses oleh STAFF (tanpa assignment region → tidak dibatasi).
    let r = app
        .post(&url)
        .token(&c.staff.token)
        .json(&json!({"clientRequestId": crid, "deliveredAt": now_ms,
                      "lines": [{"itemId": item, "deliveredQty": "3", "reasonId": fx.reason_delivery}]}))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let d = r.data();
    assert_eq!(d["id"], id.as_str());
    assert_eq!(d["status"], "delivered");
    assert_eq!(d["deliveryRevision"], 1);
    assert_eq!(d["items"][0]["deliveredQty"], "3");
    assert_eq!(d["items"][0]["shortQty"], "1");
    let (source, recorded_by): (String, i32) = sqlx::query_as(&format!(
        r#"SELECT d.source::text, d."recordedById" FROM "orderDelivery" d
             JOIN "order" o ON o.id = d."orderId" WHERE o.uuid = '{id}'::uuid"#
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(source, "mobile");
    assert_eq!(recorded_by, c.staff.id);

    // Replay kunci sama → 200 tanpa riwayat ganda; kunci lain → 409.
    let r = app
        .post(&url)
        .token(&c.staff.token)
        .json(&body)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["deliveryHistory"].as_array().unwrap().len(), 1);
    app.post(&url)
        .token(&c.staff.token)
        .json(&json!({"clientRequestId": uuid4(), "deliveredAt": now_ms}))
        .send()
        .await
        .assert_status(409);
}

#[tokio::test]
async fn mobile_orders_and_no_order() {
    let Some((app, c)) = setup().await else {
        return;
    };
    let _g = SERIAL.lock().await;
    app.exec(&format!(
        r#"UPDATE company SET "maxOrderDiscountPercent" = NULL WHERE id = {}"#,
        c.id
    ))
    .await;
    let fx = fixture(&app, &c).await;
    let r = app
        .get("/v1/mobile/orders/config")
        .token(&c.staff.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert!(r.data().get("timeZone").is_none());
    assert!(
        r.data()["orderTypes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == fx.type_auto.as_str())
    );

    let crid = uuid4();
    let body = json!({
        "clientRequestId": crid, "orderTypeId": fx.type_auto, "customerId": fx.customer, "orderDate": fx.today,
        "items": [{"productId": fx.product, "uomId": fx.pcs, "qty": "2", "unitPrice": "900"}]
    });
    let r = app
        .post("/v1/mobile/orders")
        .token(&c.staff.token)
        .json(&body)
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    let id = r.data()["id"].as_str().unwrap().to_owned();
    assert_eq!(r.data()["status"], "approved");
    assert_eq!(r.data()["source"], "mobile");
    assert_eq!(r.data()["items"][0]["priceAdjusted"], true);
    // Replay → 200, order sama.
    let r = app
        .post("/v1/mobile/orders")
        .token(&c.staff.token)
        .json(&body)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["id"], id.as_str());
    // Tanpa item → 400.
    let r = app
        .post("/v1/mobile/orders")
        .token(&c.staff.token)
        .json(&json!({"clientRequestId": uuid4(), "orderTypeId": fx.type_auto, "customerId": fx.customer,
                      "orderDate": fx.today, "items": []}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!(["items must contain at least 1 elements"])
    );
    // Harga tidak ditemukan → tidak ada order yang tertinggal.
    let crid2 = uuid4();
    let r = app
        .post("/v1/mobile/orders")
        .token(&c.staff.token)
        .json(&json!({"clientRequestId": crid2, "orderTypeId": fx.type_auto, "customerId": fx.customer,
                      "orderDate": fx.today, "items": [{"productId": fx.product, "uomId": fx.pcs, "qty": "0.5"}]}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(r.json["code"], "PRICE_NOT_FOUND");
    let left: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "order" WHERE "clientRequestId" = '{crid2}'"#
        ))
        .await;
    assert_eq!(left, 0);

    // Delta + detail.
    // Trigger `touch_updated_at` menimpa `updatedAt` dengan NOW() (zona sesi); matikan sementara.
    app.exec(&format!(
        r#"SET session_replication_role = replica;
           UPDATE "order" SET "updatedAt" = (NOW() AT TIME ZONE 'UTC') - interval '1 minute' WHERE uuid = '{id}';
           SET session_replication_role = origin;"#
    ))
    .await;
    let r = app
        .get("/v1/mobile/orders?limit=1")
        .token(&c.staff.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.data().as_array().unwrap().len(), 1);
    assert!(r.data()[0]["items"].is_array());
    assert!(r.data()[0].get("deliveryHistory").is_none());
    let r = app
        .get(&format!("/v1/mobile/orders/{id}"))
        .token(&c.staff.token)
        .send()
        .await;
    r.assert_status(200);
    app.get(&format!("/v1/mobile/orders/{id}"))
        .token(&c.staff2.token)
        .send()
        .await
        .assert_status(404);
    app.get("/v1/mobile/orders/not-a-uuid")
        .token(&c.staff.token)
        .send()
        .await
        .assert_status(400);

    // No-order.
    let (_, visit): (i32, String) = sqlx::query_as(&format!(
        r#"INSERT INTO "storeVisit" ("companyId", "storeId", "userId", "visitNumber", "clientRequestId",
             "checkInAt", "checkInLat", "checkInLng", "checkInAccuracy", "checkInDistance", "checkInGpsValidation")
           VALUES ({}, {}, {}, 'IT-{}', gen_random_uuid(), NOW(), -6.2, 106.8, 5, 0, 'valid') RETURNING id, uuid::text"#,
        c.id,
        fx.store_id,
        c.staff.id,
        tag()
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    app.get(&format!("/v1/admin/store-visits/{visit}/no-order"))
        .token(&c.admin.token)
        .send()
        .await
        .assert_status(404);
    let r = app
        .post(&format!("/v1/mobile/store-visits/{visit}/no-order"))
        .token(&c.staff2.token)
        .json(&json!({"reasonId": fx.reason_no_order}))
        .send()
        .await;
    assert_eq!(r.json["code"], "STORE_VISIT_OWNERSHIP_MISMATCH");
    let r = app
        .post(&format!("/v1/mobile/store-visits/{visit}/no-order"))
        .token(&c.staff.token)
        .json(&json!({"reasonId": fx.reason_cancel}))
        .send()
        .await;
    assert_eq!(r.json["code"], "REASON_TYPE_MISMATCH");
    let r = app
        .post(&format!("/v1/mobile/store-visits/{visit}/no-order"))
        .token(&c.staff.token)
        .json(&json!({"reasonId": fx.reason_no_order, "note": "tutup"}))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    assert_eq!(r.data()["storeVisitId"], visit.as_str());
    assert_eq!(r.data()["reason"]["name"], "Tutup");
    let r = app
        .post(&format!("/v1/mobile/store-visits/{visit}/no-order"))
        .token(&c.staff.token)
        .json(&json!({"reasonId": fx.reason_no_order}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "STORE_VISIT_NO_ORDER_EXISTS");
    let r = app
        .get(&format!("/v1/admin/store-visits/{visit}/no-order"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["note"], "tutup");
    app.post("/v1/mobile/orders")
        .json(&body)
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
async fn field_policy_censors_orders_and_items() {
    let Some((app, c)) = setup().await else {
        return;
    };
    let _g = SERIAL.lock().await;
    app.exec(&format!(
        r#"UPDATE company SET "maxOrderDiscountPercent" = NULL WHERE id = {}"#,
        c.id
    ))
    .await;
    let fx = fixture(&app, &c).await;
    let id = create_draft(&app, &c, &fx, &fx.type_auto).await;
    put_items(
        &app,
        &c,
        &id,
        json!([{"productId": fx.product, "uomId": fx.pcs, "qty": "2", "discountPercent": "10"}]),
    )
    .await
    .assert_status(200);
    let role: i32 = app
        .scalar(&format!(
            r#"SELECT "accessLevelId" FROM "userAccessLevel" WHERE "userId" = {} AND "companyId" = {} AND status = 1 LIMIT 1"#,
            c.manager.id, c.id
        ))
        .await;
    app.exec(&format!(
        r#"DELETE FROM "tableFieldVisibility" WHERE "companyId" = {cid};
           INSERT INTO "tableFieldVisibility" ("companyId", "accessLevelId", "tableKey", "columnKey", mode) VALUES
             ({cid}, {role}, 'orders', 'grandTotal', 'hidden'),
             ({cid}, {role}, 'order_items', 'unitPrice', 'masked'),
             ({cid}, {role}, 'order_items', 'discountPercent', 'masked');"#,
        cid = c.id
    ))
    .await;
    mz_table_fields::policy::invalidate_company_local(c.id);

    let r = app
        .get(&format!("/v1/admin/orders/{id}"))
        .token(&c.manager.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["grandTotal"], Value::Null);
    assert_eq!(r.data()["subtotal"], "2000");
    assert_eq!(r.data()["items"][0]["unitPrice"], Value::Null);
    assert_eq!(r.json["fieldPolicy"]["tableKey"], "orders");
    assert_eq!(r.json["fieldPolicy"]["hidden"], json!(["grandTotal"]));
    let r = app
        .get("/v1/admin/orders?sortBy=grandTotal")
        .token(&c.manager.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["code"], "FIELD_NOT_SORTABLE");
    // Diskon tersensor dibawa dari baris tersimpan (N1), bukan dari body.
    let r = put_items(
        &app,
        &c,
        &id,
        json!([{"productId": fx.product, "uomId": fx.pcs, "qty": "3", "discountPercent": "15"}]),
    )
    .await;
    r.assert_status(200);
    let r = app
        .put(&format!("/v1/admin/orders/{id}/items"))
        .token(&c.manager.token)
        .json(&json!({"items": [{"productId": fx.product, "uomId": fx.pcs, "qty": "4", "discountPercent": "0"}]}))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.data()["items"][0]["discountPercent"], Value::Null);
    let stored: String = app
        .scalar(&format!(
            r#"SELECT i."discountPercent"::text FROM "orderItem" i JOIN "order" o ON o.id = i."orderId" WHERE o.uuid = '{id}'"#
        ))
        .await;
    assert_eq!(stored, "15.00");
    // Export: kolom `hidden` dibuang.
    let r = app
        .get(&format!(
            "/v1/admin/orders/export?format=csv&storeId={}",
            fx.store
        ))
        .token(&c.manager.token)
        .send()
        .await;
    r.assert_status(200);
    let header = r.text().lines().next().unwrap().to_owned();
    assert!(!header.contains("grandTotal"), "{header}");
    assert!(!header.contains("shortDeliveredValue"));

    app.exec(&format!(
        r#"DELETE FROM "tableFieldVisibility" WHERE "companyId" = {}"#,
        c.id
    ))
    .await;
    mz_table_fields::policy::invalidate_company_local(c.id);
}
