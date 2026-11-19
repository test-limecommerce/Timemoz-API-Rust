//! Uji integrasi price list (11 route) + mobile catalog sync (1 route) terhadap Postgres uji.
//! Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-products --test it_products_catalog`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{Ctx, assert_key_order, code_of, messages, setup, tag};
use serde_json::{Value, json};

const RANDOM_UUID: &str = "4d1a2b3c-5e6f-4a7b-8c9d-0e1f2a3b4c5d";

/// Produk fixture (SQL langsung): kategori baru + base PCS + BOX (multiply 24).
struct Fx {
    product_id: i32,
    product_uuid: String,
    sku: String,
    pcs: String,
    boxu: String,
    kg: String,
}

async fn product_fixture(c: &Ctx) -> Fx {
    let t = tag();
    let cid = c.fx.id;
    let uid = c.fx.admin.id;
    let cat: i32 = c
        .app
        .scalar(&format!(
            r#"INSERT INTO "productCategory" ("companyId", code, name, "createdById")
               VALUES ({cid}, 'C{t}', 'Cat {t}', {uid}) RETURNING id"#
        ))
        .await;
    let pcs: i32 = c
        .app
        .scalar(&format!(
            r#"SELECT id FROM uom WHERE "companyId" = {cid} AND code = 'PCS'"#
        ))
        .await;
    let boxu: i32 = c
        .app
        .scalar(&format!(
            r#"SELECT id FROM uom WHERE "companyId" = {cid} AND code = 'BOX'"#
        ))
        .await;
    let sku = format!("SKU-{t}");
    let pid: i32 = c
        .app
        .scalar(&format!(
            r#"INSERT INTO product ("companyId", sku, name, "categoryId", "baseUomId", "createdById", "customFields")
               VALUES ({cid}, '{sku}', 'Produk {t}', {cat}, {pcs}, {uid}, '{{}}') RETURNING id"#
        ))
        .await;
    c.app
        .exec(&format!(
            r#"INSERT INTO "productUom" ("productId", "uomId", "conversionRate", operator, factor, "isBase")
               VALUES ({pid}, {pcs}, 1, 'multiply', 1, true), ({pid}, {boxu}, 24, 'multiply', 24, false);
               INSERT INTO "productBarcode" ("companyId", "productId", barcode) VALUES ({cid}, {pid}, 'BC{t}');"#
        ))
        .await;
    let u = |code: &'static str| {
        let app = &c.app;
        async move {
            app.scalar::<String>(&format!(
                r#"SELECT uuid::text FROM uom WHERE "companyId" = {cid} AND code = '{code}'"#
            ))
            .await
        }
    };
    Fx {
        product_id: pid,
        product_uuid: c
            .app
            .scalar(&format!("SELECT uuid::text FROM product WHERE id = {pid}"))
            .await,
        sku,
        pcs: u("PCS").await,
        boxu: u("BOX").await,
        kg: u("KG").await,
    }
}

async fn new_price_list(c: &Ctx, body: Value) -> Value {
    let r = c
        .app
        .post("/v1/admin/price-lists")
        .token(&c.fx.admin.token)
        .json(&body)
        .send()
        .await;
    r.assert_no_sql_error();
    r.assert_status(201);
    r.data().clone()
}

async fn new_customer(c: &Ctx) -> String {
    let t = tag();
    c.app
        .scalar(&format!(
            r#"INSERT INTO customer ("companyId", code, name, "createdById")
               VALUES ({}, 'CU{t}', 'Cust {t}', {}) RETURNING uuid::text"#,
            c.fx.id, c.fx.admin.id
        ))
        .await
}

// ───────────────────────── price list CRUD ─────────────────────────

#[tokio::test]
async fn price_list_crud() {
    let Some(c) = setup().await else { return };
    c.app
        .exec(&format!(
            r#"UPDATE "priceList" SET "isDefault" = false WHERE "companyId" = {}"#,
            c.fx.id
        ))
        .await;
    let code = format!("pl-{}", tag());
    let r = c
        .app
        .post("/v1/admin/price-lists")
        .token(&c.fx.admin.token)
        .json(&json!({ "code": code, "name": "General Trade", "startDate": "2026-01-01" }))
        .send()
        .await;
    r.assert_no_sql_error().assert_status(201);
    assert_key_order(
        &r.text(),
        &[
            "id",
            "code",
            "name",
            "currencyCode",
            "startDate",
            "endDate",
            "isDefault",
            "isActive",
            "isActiveToday",
            "createdAt",
            "updatedAt",
        ],
    );
    let pl = r.data().clone();
    assert_eq!(pl["code"], code.to_uppercase());
    assert_eq!(pl["startDate"], "2026-01-01");
    assert_eq!(pl["endDate"], Value::Null);
    assert_eq!(pl["isActiveToday"], true);
    assert!(pl["createdAt"].is_i64());
    let id = pl["id"].as_str().unwrap().to_owned();

    // kode ganda → 409
    let r = c
        .app
        .post("/v1/admin/price-lists")
        .token(&c.fx.admin.token)
        .json(&json!({ "code": code, "name": "Dup" }))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(code_of(&r), "PRICE_LIST_CODE_TAKEN");

    // dua default → 409 default conflict
    new_price_list(
        &c,
        json!({ "code": format!("D1{}", tag()), "name": "D1", "isDefault": true }),
    )
    .await;
    let r = c
        .app
        .post("/v1/admin/price-lists")
        .token(&c.fx.admin.token)
        .json(&json!({ "code": format!("D2{}", tag()), "name": "D2", "isDefault": true }))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(code_of(&r), "PRICE_LIST_DEFAULT_CONFLICT");

    // validasi
    let r = c
        .app
        .post("/v1/admin/price-lists")
        .token(&c.fx.admin.token)
        .json(&json!({ "name": "", "endDate": "1/1/2026" }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec![
            "code must be 1-32 characters of uppercase letters, digits, underscore, or hyphen",
            "code must be a string",
            "name must be longer than or equal to 1 characters",
            "endDate must be a YYYY-MM-DD date",
        ]
    );
    let r = c
        .app
        .post("/v1/admin/price-lists")
        .token(&c.fx.admin.token)
        .json(&json!({ "code": format!("X{}", tag()), "name": "X", "startDate": "2026-02-30" }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(code_of(&r), "INVALID_DATE_RANGE");

    // role STAFF ditolak
    c.app
        .get("/v1/admin/price-lists")
        .token(&c.fx.staff.token)
        .send()
        .await
        .assert_status(401); // RolesGuard: 401 "Access denied: insufficient permissions"

    // list + search
    let r = c
        .app
        .get(&format!("/v1/admin/price-lists?search={code}&limit=5"))
        .token(&c.fx.manager.token)
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.json["meta"]["total"], 1);
    assert_eq!(r.json["meta"]["limit"], 5);
    assert_eq!(r.json["data"][0]["id"], id.as_str());
    let r = c
        .app
        .get("/v1/admin/price-lists?limit=101")
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(messages(&r), vec!["limit must not be greater than 100"]);

    // detail / 404 / uuid rusak = 500 (P2023 di Node)
    c.app
        .get(&format!("/v1/admin/price-lists/{id}"))
        .token(&c.fx.admin.token)
        .send()
        .await
        .assert_status(200);
    let r = c
        .app
        .get(&format!("/v1/admin/price-lists/{RANDOM_UUID}"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(code_of(&r), "PRICE_LIST_NOT_FOUND");
    c.app
        .get("/v1/admin/price-lists/nope")
        .token(&c.fx.admin.token)
        .send()
        .await
        .assert_status(500);

    // update: tanggal → endDate sebelum startDate ditolak
    let r = c
        .app
        .patch(&format!("/v1/admin/price-lists/{id}"))
        .token(&c.fx.admin.token)
        .json(&json!({ "endDate": "2025-12-31" }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(code_of(&r), "INVALID_DATE_RANGE");
    let r = c
        .app
        .patch(&format!("/v1/admin/price-lists/{id}"))
        .token(&c.fx.admin.token)
        .json(
            &json!({ "name": "GT 2", "startDate": null, "currencyCode": "idr", "isActive": false }),
        )
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.data()["name"], "GT 2");
    assert_eq!(r.data()["startDate"], Value::Null);
    assert_eq!(r.data()["currencyCode"], "IDR");
    assert_eq!(r.data()["isActiveToday"], false);
    assert!(r.data()["updatedAt"].is_i64());

    // hapus → 404 sesudahnya
    let r = c
        .app
        .delete(&format!("/v1/admin/price-lists/{id}"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json, json!({ "data": { "id": id, "deleted": true } }));
    c.app
        .get(&format!("/v1/admin/price-lists/{id}"))
        .token(&c.fx.admin.token)
        .send()
        .await
        .assert_status(404);

    // audit price list tercatat
    let n: i64 = c
        .app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "auditLog" WHERE "entityType"::text = 'priceList' AND "entityUuid"::text = '{id}'"#
        ))
        .await;
    assert!(n >= 1, "audit priceList kosong");
}

// ───────────────────────── items ─────────────────────────

#[tokio::test]
async fn price_list_items() {
    let Some(c) = setup().await else { return };
    let fx = product_fixture(&c).await;
    let pl = new_price_list(
        &c,
        json!({ "code": format!("I{}", tag()), "name": "Items" }),
    )
    .await;
    let id = pl["id"].as_str().unwrap();
    let url = format!("/v1/admin/price-lists/{id}/items");

    // validasi body
    let r = c
        .app
        .put(&url)
        .token(&c.fx.admin.token)
        .json(&json!({ "items": [] }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(messages(&r), vec!["items must contain at least 1 elements"]);
    let r = c
        .app
        .put(&url)
        .token(&c.fx.admin.token)
        .json(&json!({ "items": [{ "productId": "x", "uomId": fx.pcs, "price": "1.234" }] }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec![
            "items.0.productId must be a UUID",
            "items.0.price must be a non-negative number with up to 2 decimal places",
        ]
    );

    // buat dua tier + satu tier BOX; urutan respons = urutan input
    let r = c
        .app
        .put(&url)
        .token(&c.fx.admin.token)
        .json(&json!({ "items": [
            { "productId": fx.product_uuid, "uomId": fx.pcs, "minQty": "10", "maxQty": null, "price": "9000.50" },
            { "productId": fx.product_uuid, "uomId": fx.pcs, "maxQty": "9.9999", "price": "10000" },
            { "productId": fx.product_uuid, "uomId": fx.boxu, "price": "200000.00" },
        ] }))
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    let items = r.json["data"].as_array().unwrap().clone();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["minQty"], "10");
    assert_eq!(items[0]["maxQty"], Value::Null);
    assert_eq!(items[0]["price"], "9000.5");
    assert_eq!(items[1]["minQty"], "1");
    assert_eq!(items[1]["maxQty"], "9.9999");
    assert_eq!(items[2]["uom"]["code"], "BOX");
    assert_eq!(items[0]["product"]["sku"], fx.sku.as_str());
    assert_key_order(
        &r.text(),
        &[
            "id",
            "product",
            "uom",
            "minQty",
            "maxQty",
            "price",
            "createdAt",
            "updatedAt",
        ],
    );

    // tumpang tindih dengan yang ada → 409
    let r = c
        .app
        .put(&url)
        .token(&c.fx.admin.token)
        .json(&json!({ "items": [{ "productId": fx.product_uuid, "uomId": fx.pcs, "minQty": "5", "maxQty": "20", "price": "1" }] }))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(code_of(&r), "PRICE_TIER_OVERLAP");
    // tumpang tindih di dalam satu batch → 409
    let r = c
        .app
        .put(&url)
        .token(&c.fx.admin.token)
        .json(&json!({ "items": [
            { "productId": fx.product_uuid, "uomId": fx.boxu, "minQty": "2", "maxQty": "3", "price": "1" },
            { "productId": fx.product_uuid, "uomId": fx.boxu, "minQty": "3", "price": "1" },
        ] }))
        .send()
        .await;
    r.assert_status(409);
    // UOM bukan milik produk → 422
    let r = c
        .app
        .put(&url)
        .token(&c.fx.admin.token)
        .json(&json!({ "items": [{ "productId": fx.product_uuid, "uomId": fx.kg, "price": "1" }] }))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "UOM_NOT_IN_PRODUCT");
    // qty tidak sah → 422
    let r = c
        .app
        .put(&url)
        .token(&c.fx.admin.token)
        .json(&json!({ "items": [{ "productId": fx.product_uuid, "uomId": fx.boxu, "minQty": "0", "price": "1" }] }))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "PRICE_LIST_ITEM_INVALID_QTY");
    // produk tak dikenal → 404
    let r = c
        .app
        .put(&url)
        .token(&c.fx.admin.token)
        .json(&json!({ "items": [{ "productId": RANDOM_UUID, "uomId": fx.pcs, "price": "1" }] }))
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(code_of(&r), "PRODUCT_NOT_FOUND");

    // update tier lewat `id` (ganti harga + rentang yang tadinya bentrok dengan dirinya sendiri)
    let tier1 = items[1]["id"].as_str().unwrap().to_owned();
    let r = c
        .app
        .put(&url)
        .token(&c.fx.admin.token)
        .json(&json!({ "items": [{ "id": tier1, "productId": fx.product_uuid, "uomId": fx.pcs, "minQty": "1", "maxQty": "5", "price": "11000" }] }))
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.json["data"][0]["id"], tier1.as_str());
    assert_eq!(r.json["data"][0]["price"], "11000");
    assert_eq!(r.json["data"][0]["maxQty"], "5");

    // list + filter produk
    let r = c
        .app
        .get(&format!("{url}?productId={}&limit=2", fx.product_uuid))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(
        r.json["meta"],
        json!({ "total": 3, "page": 1, "limit": 2, "totalPages": 2 })
    );
    let r = c
        .app
        .get(&format!("{url}?productId={RANDOM_UUID}"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(code_of(&r), "PRODUCT_NOT_FOUND");

    // get / delete item
    let r = c
        .app
        .get(&format!("{url}/{tier1}"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["uom"]["code"], "PCS");
    let r = c
        .app
        .delete(&format!("{url}/{tier1}"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json, json!({ "data": { "id": tier1, "deleted": true } }));
    let r = c
        .app
        .get(&format!("{url}/{tier1}"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(code_of(&r), "PRICE_LIST_ITEM_NOT_FOUND");
    c.app
        .delete(&format!("{url}/{RANDOM_UUID}"))
        .token(&c.fx.admin.token)
        .send()
        .await
        .assert_status(404);
    // STAFF ditolak
    c.app
        .put(&url)
        .token(&c.fx.staff.token)
        .json(&json!({ "items": [] }))
        .send()
        .await
        .assert_status(401);
    let n: i64 = c
        .app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "priceListItem" WHERE "productId" = {} AND "deletedAt" IS NULL"#,
            fx.product_id
        ))
        .await;
    assert_eq!(n, 2);
}

// ───────────────────────── customers ─────────────────────────

#[tokio::test]
async fn price_list_customers() {
    let Some(c) = setup().await else { return };
    let pl = new_price_list(&c, json!({ "code": format!("C{}", tag()), "name": "Cust" })).await;
    let id = pl["id"].as_str().unwrap();
    let (c1, c2) = (new_customer(&c).await, new_customer(&c).await);
    let url = format!("/v1/admin/price-lists/{id}/customers");

    let r = c
        .app
        .patch(&url)
        .token(&c.fx.admin.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec![
            "each value in customerIds must be a UUID",
            "customerIds must contain no more than 500 elements",
            "customerIds must be an array",
        ]
    );

    let r = c
        .app
        .patch(&url)
        .token(&c.fx.admin.token)
        .json(&json!({ "customerIds": [c1, c2, RANDOM_UUID] }))
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(
        r.json,
        json!({ "data": { "assigned": 2, "unassigned": 0, "notFound": [RANDOM_UUID] } })
    );

    let r = c
        .app
        .get(&format!("{url}?limit=500"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.json["meta"]["total"], 2);
    assert_key_order(
        &r.text(),
        &[
            "id",
            "code",
            "name",
            "type",
            "group",
            "channel",
            "subChannel",
            "priceList",
            "taxNumber",
            "phone",
            "isActive",
            "storeCount",
            "createdAt",
            "updatedAt",
        ],
    );
    assert_eq!(r.json["data"][0]["priceList"]["id"], id);
    assert_eq!(r.json["data"][0]["storeCount"], 0);
    c.app
        .get(&format!("{url}?limit=501"))
        .token(&c.fx.admin.token)
        .send()
        .await
        .assert_status(400);

    let r = c
        .app
        .patch(&url)
        .token(&c.fx.admin.token)
        .json(&json!({ "customerIds": [c1] }))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.json["data"],
        json!({ "assigned": 1, "unassigned": 1, "notFound": [] })
    );
    let r = c
        .app
        .patch(&url)
        .token(&c.fx.admin.token)
        .json(&json!({ "customerIds": [] }))
        .send()
        .await;
    assert_eq!(
        r.json["data"],
        json!({ "assigned": 0, "unassigned": 1, "notFound": [] })
    );
    c.app
        .get(&format!("/v1/admin/price-lists/{RANDOM_UUID}/customers"))
        .token(&c.fx.admin.token)
        .send()
        .await
        .assert_status(404);
}

// ───────────────────────── mobile catalog ─────────────────────────

#[tokio::test]
async fn mobile_catalog_sync() {
    let Some(c) = setup().await else { return };
    let fx = product_fixture(&c).await;
    let pl = new_price_list(
        &c,
        json!({ "code": format!("M{}", tag()), "name": "Mobile" }),
    )
    .await;
    let r = c
        .app
        .put(&format!("/v1/admin/price-lists/{}/items", pl["id"].as_str().unwrap()))
        .token(&c.fx.admin.token)
        .json(&json!({ "items": [{ "productId": fx.product_uuid, "uomId": fx.pcs, "price": "5000" }] }))
        .send()
        .await;
    r.assert_status(200);
    // safety lag 5 dtk: baris yang baru ditulis belum ikut.
    tokio::time::sleep(std::time::Duration::from_millis(6_500)).await;

    let r = c
        .app
        .get("/v1/mobile/catalog/sync?limit=abc")
        .token(&c.fx.staff.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec![
            "limit must not be greater than 2000",
            "limit must not be less than 1",
            "limit must be an integer number"
        ]
    );

    let r = c
        .app
        .get("/v1/mobile/catalog/sync?limit=2000")
        .token(&c.fx.staff.token)
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    let etag = r.headers.get("etag").unwrap().to_str().unwrap().to_owned();
    let body = r.json.clone();
    assert_eq!(body["mode"], "full");
    assert_eq!(
        etag,
        format!("\"{}\"", body["catalogToken"].as_str().unwrap())
    );
    assert_key_order(
        &r.text(),
        &[
            "epoch",
            "mode",
            "syncedAt",
            "catalogToken",
            "hasMore",
            "nextCursor",
            "masters",
        ],
    );
    let product = body["products"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == fx.product_uuid.as_str())
        .unwrap_or_else(|| panic!("produk fixture tidak ada: {}", r.text()))
        .clone();
    assert_eq!(product["sku"], fx.sku.as_str());
    assert!(product.get("subCategoryId").is_none(), "null harus dibuang");
    assert_eq!(product["uoms"][1]["rate"], "24");
    assert_eq!(product["uoms"][0]["factor"], "1");
    assert_eq!(product["barcodes"].as_array().unwrap().len(), 1);
    assert!(
        body["masters"]["uoms"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["code"] == "PCS")
    );
    let snap = body["priceLists"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == pl["id"])
        .unwrap()
        .clone();
    assert_eq!(snap["items"][0]["price"], "5000");
    assert!(snap["items"][0].get("maxQty").is_none());
    assert!(snap.get("startDate").is_none());

    // 304 dengan If-None-Match (lemah & daftar)
    let r = c
        .app
        .get("/v1/mobile/catalog/sync")
        .token(&c.fx.staff.token)
        .header("if-none-match", &format!("W/\"zzz\", W/{etag}"))
        .send()
        .await;
    r.assert_status(304);
    assert!(r.bytes.is_empty());
    assert_eq!(r.headers.get("etag").unwrap().to_str().unwrap(), etag);

    // delta dari nextCursor: tidak ada yang berubah → tanpa master/priceLists, produk kosong
    let cursor = body["nextCursor"].as_str().unwrap();
    let r = c
        .app
        .get(&format!("/v1/mobile/catalog/sync?cursor={cursor}"))
        .token(&c.fx.staff.token)
        .send()
        .await;
    r.assert_no_sql_error().assert_status(200);
    assert_eq!(r.json["mode"], "delta");
    assert!(r.json.get("masters").is_none());
    assert!(r.json.get("priceLists").is_none());
    assert_eq!(r.json["products"], json!([]));
    assert_eq!(r.json["productsDeleted"], json!([]));

    // cursor rusak = full resync, bukan 400
    let r = c
        .app
        .get("/v1/mobile/catalog/sync?cursor=garbage&limit=1")
        .token(&c.fx.staff.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["mode"], "full");
    assert_eq!(r.json["products"].as_array().unwrap().len(), 1);
    let full_count = body["products"].as_array().unwrap().len()
        + body["productsDeleted"].as_array().unwrap().len();
    assert_eq!(r.json["hasMore"], full_count > 1);

    // tanpa token → 401
    c.app
        .get("/v1/mobile/catalog/sync")
        .send()
        .await
        .assert_status(401);
}
