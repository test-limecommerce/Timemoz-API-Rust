//! Master produk, custom field produk, CRUD produk + list/cf/summary/export, gambar produk.

use std::time::Duration;

use serde_json::{Value, json};

use crate::common::*;

async fn audit_count(c: &Ctx, entity: &str, id: i32, at_least: i64) -> i64 {
    let mut n = 0;
    for _ in 0..50 {
        n = c
            .app
            .scalar::<i64>(&format!(
                r#"SELECT COUNT(*) FROM "auditLog" WHERE "entityType"::text = '{entity}' AND "entityId" = {id}"#
            ))
            .await;
        if n >= at_least {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    n
}

// ───────────────────────── master ─────────────────────────

#[tokio::test]
async fn masters_crud_dan_tolak() {
    let Some(c) = setup().await else { return };
    let t = tag();
    let admin = &c.fx.admin.token;

    // create kategori: kode di-uppercase, urutan kunci entitas.
    let r = c
        .app
        .post("/v1/admin/product-masters/categories")
        .token(admin)
        .json(&json!({ "code": format!("cat{t}"), "name": "Beverages", "sortOrder": "3" }))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    let cat = r.data().clone();
    assert_eq!(cat["code"], format!("CAT{t}"));
    assert_eq!(cat["sortOrder"], 3);
    assert_key_order(
        &r.text(),
        &[
            "id",
            "code",
            "name",
            "isActive",
            "description",
            "sortOrder",
            "createdAt",
            "updatedAt",
        ],
    );
    let cat_id: i32 = c
        .app
        .scalar(&format!(
            r#"SELECT id FROM "productCategory" WHERE uuid = '{}'"#,
            cat["id"].as_str().unwrap()
        ))
        .await;
    assert!(audit_count(&c, "productCategory", cat_id, 1).await >= 1);

    // duplikat kode → 409.
    let r = c
        .app
        .post("/v1/admin/product-masters/categories")
        .token(admin)
        .json(&json!({ "code": format!("CAT{t}"), "name": "x" }))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(code_of(&r), "PRODUCT_MASTER_CODE_TAKEN");

    // kind tak dikenal → 400 ParseEnumPipe.
    let r = c
        .app
        .get("/v1/admin/product-masters/foo")
        .token(admin)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        "Validation failed (enum string is expected)"
    );

    // STAFF ditolak RolesGuard.
    let r = c
        .app
        .get("/v1/admin/product-masters/categories")
        .token(&c.fx.staff.token)
        .send()
        .await;
    // RolesGuard Node: 401 "Access denied: insufficient permissions".
    r.assert_status(401);

    // validasi body.
    let r = c
        .app
        .post("/v1/admin/product-masters/brands")
        .token(admin)
        .json(&json!({ "code": "bad code" }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec![
            "code must be 1-32 characters of uppercase letters, digits, underscore, or hyphen",
            "name must be longer than or equal to 1 characters",
            "name must be a string",
        ]
    );

    // field milik kind lain → 422; tax tanpa rate → 422.
    let r = c
        .app
        .post("/v1/admin/product-masters/brands")
        .token(admin)
        .json(&json!({ "code": format!("B{t}"), "name": "x", "rate": "11" }))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "PRODUCT_MASTER_INVALID_FIELD");
    let r = c
        .app
        .post("/v1/admin/product-masters/taxes")
        .token(admin)
        .json(&json!({ "code": format!("T{t}"), "name": "PPN" }))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "TAX_RATE_REQUIRED");
    let tax = new_master(
        &c,
        "taxes",
        json!({ "code": format!("T{t}"), "name": "PPN", "rate": "11.0000", "isInclusive": "true" }),
    )
    .await;
    assert_eq!(tax["rate"], "11");
    assert_eq!(tax["isInclusive"], true);

    // uom: default factor tidak sah → 422.
    let r = c
        .app
        .post("/v1/admin/product-masters/uoms")
        .token(admin)
        .json(&json!({ "code": format!("U{t}"), "name": "Gram", "defaultOperator": "divide", "defaultFactor": "3000000" }))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "UOM_INVALID_DEFAULT_FACTOR");
    let uom = new_master(&c, "uoms", json!({ "code": format!("U{t}"), "name": "Dus", "uomType": "unit", "defaultFactor": "24.000000" })).await;
    assert_eq!(uom["defaultFactor"], "24");
    assert_eq!(uom["defaultOperator"], "multiply");

    // sub kategori + ref kategori.
    let sub = new_master(
        &c,
        "sub-categories",
        json!({ "code": format!("S{t}"), "name": "Soda", "categoryId": cat["id"] }),
    )
    .await;
    assert_eq!(sub["category"]["id"], cat["id"]);
    let r = c
        .app
        .post("/v1/admin/product-masters/sub-categories")
        .token(admin)
        .json(&json!({ "code": format!("S2{t}"), "name": "Soda" }))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "SUBCATEGORY_WITHOUT_CATEGORY");

    // list + search + filter kategori.
    let r = c
        .app
        .get(&format!(
            "/v1/admin/product-masters/sub-categories?search=S{t}&categoryId={}&limit=5",
            cat["id"].as_str().unwrap()
        ))
        .token(&c.fx.manager.token)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.json["meta"]["total"], 1);
    assert_eq!(r.json["data"][0]["category"]["code"], format!("CAT{t}"));
    let r = c
        .app
        .get("/v1/admin/product-masters/brands?limit=500")
        .token(admin)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(messages(&r), vec!["limit must not be greater than 100"]);

    // detail, update, 404, arsip.
    let path = format!(
        "/v1/admin/product-masters/sub-categories/{}",
        sub["id"].as_str().unwrap()
    );
    let r = c.app.get(&path).token(admin).send().await;
    r.assert_ok();
    assert_eq!(r.data()["name"], "Soda");
    let r = c
        .app
        .patch(&path)
        .token(admin)
        .json(&json!({ "name": "Soda 2", "description": null, "isActive": true }))
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.data()["name"], "Soda 2");
    assert_eq!(r.data()["category"]["id"], cat["id"]);
    let r = c
        .app
        .get(&format!(
            "/v1/admin/product-masters/brands/{}",
            uuid::Uuid::new_v4()
        ))
        .token(admin)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(r.json["message"], "Product brand not found");
    let r = c.app.delete(&path).token(admin).send().await;
    r.assert_ok();
    assert_eq!(r.data(), &json!({ "id": sub["id"], "archived": true }));
}

// ───────────────────────── custom field ─────────────────────────

#[tokio::test]
async fn custom_fields_crud() {
    let Some(c) = setup().await else { return };
    let t = tag().to_lowercase();
    let admin = &c.fx.admin.token;
    let epoch0: i32 = c
        .app
        .scalar(&format!(
            r#"SELECT "catalogEpoch" FROM company WHERE id = {}"#,
            c.fx.id
        ))
        .await;
    let key = format!("storage_{t}");
    let r = c
        .app
        .post("/v1/admin/product-custom-fields")
        .token(admin)
        .json(&json!({ "key": key, "label": "Storage", "type": "select",
                       "options": [{ "value": "dry", "label": "Dry" }] }))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    let cf = r.data().clone();
    assert_key_order(
        &r.text(),
        &[
            "id",
            "key",
            "label",
            "type",
            "options",
            "isRequired",
            "isSensitive",
            "sortOrder",
            "isActive",
            "createdAt",
            "updatedAt",
        ],
    );
    let epoch1: i32 = c
        .app
        .scalar(&format!(
            r#"SELECT "catalogEpoch" FROM company WHERE id = {}"#,
            c.fx.id
        ))
        .await;
    assert_eq!(epoch1, epoch0 + 1);

    let r = c
        .app
        .post("/v1/admin/product-custom-fields")
        .token(admin)
        .json(&json!({ "key": key, "label": "Storage", "type": "text" }))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(code_of(&r), "CUSTOM_FIELD_KEY_TAKEN");
    let r = c
        .app
        .post("/v1/admin/product-custom-fields")
        .token(admin)
        .json(&json!({ "key": format!("x_{t}"), "label": "X", "type": "select" }))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "CUSTOM_FIELD_OPTIONS_REQUIRED");

    let path = format!(
        "/v1/admin/product-custom-fields/{}",
        cf["id"].as_str().unwrap()
    );
    let r = c
        .app
        .patch(&path)
        .token(admin)
        .json(&json!({ "type": "text" }))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "CUSTOM_FIELD_IMMUTABLE");
    let r = c
        .app
        .patch(&path)
        .token(admin)
        .json(&json!({ "options": [{ "value": "cold", "label": "Cold" }] }))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "CUSTOM_FIELD_OPTIONS_IMMUTABLE");
    let r = c
        .app
        .patch(&path)
        .token(admin)
        .json(&json!({ "label": "Penyimpanan", "options": [{ "value": "dry", "label": "Dry" }, { "value": "cold", "label": "Cold" }] }))
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.data()["options"][1]["value"], "cold");

    let r = c
        .app
        .get(&format!("/v1/admin/product-custom-fields?search={key}"))
        .token(admin)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.json["meta"]["total"], 1);
    let r = c.app.get(&path).token(admin).send().await;
    r.assert_ok();
    assert_eq!(r.data()["label"], "Penyimpanan");
    let r = c.app.delete(&path).token(admin).send().await;
    r.assert_ok();
    assert_eq!(r.data()["archived"], true);
    let r = c
        .app
        .get(&format!(
            "/v1/admin/product-custom-fields/{}",
            uuid::Uuid::new_v4()
        ))
        .token(admin)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(code_of(&r), "CUSTOM_FIELD_NOT_FOUND");
}

// ───────────────────────── produk ─────────────────────────

struct Fx {
    cat: Value,
    box_uom: Value,
    cf_key: String,
}

async fn product_fixture(c: &Ctx) -> Fx {
    let t = tag();
    let cat = new_master(
        c,
        "categories",
        json!({ "code": format!("PC{t}"), "name": format!("Cat {t}") }),
    )
    .await;
    let box_uom = new_master(
        c,
        "uoms",
        json!({ "code": format!("BX{t}"), "name": "Box", "defaultFactor": "24" }),
    )
    .await;
    let cf_key = format!("shelf_{}", t.to_lowercase());
    let r = c
        .app
        .post("/v1/admin/product-custom-fields")
        .token(&c.fx.admin.token)
        .json(&json!({ "key": cf_key, "label": "Shelf life", "type": "number" }))
        .send()
        .await;
    r.assert_status(201);
    Fx {
        cat,
        box_uom,
        cf_key,
    }
}

#[tokio::test]
async fn products_crud_list_export() {
    let Some(c) = setup().await else { return };
    let fx = product_fixture(&c).await;
    let t = tag();
    let admin = &c.fx.admin.token;
    let sku = format!("SKU-{t}");
    let barcode = format!("BC{t}");

    // create: base PCS bawaan + BOX dari default master.
    let r = c
        .app
        .post("/v1/admin/products")
        .token(admin)
        .json(&json!({
            "sku": sku, "name": "Coffee ABC", "categoryId": fx.cat["id"], "weight": "20.5000",
            "uoms": [{ "uomId": fx.box_uom["id"] }],
            "barcodes": [{ "barcode": barcode }],
            "customFields": { fx.cf_key.clone(): 365 },
        }))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    let p = r.data().clone();
    assert_key_order(
        &r.text(),
        &[
            "id",
            "sku",
            "name",
            "description",
            "category",
            "subCategory",
            "brand",
            "principal",
            "baseUom",
            "taxRate",
            "weight",
            "volume",
            "imageUrl",
            "isOrderable",
            "isActive",
            "uoms",
            "barcodes",
            "customFields",
            "createdAt",
            "updatedAt",
        ],
    );
    assert_eq!(p["weight"], "20.5");
    assert_eq!(p["baseUom"]["code"], "PCS");
    assert_eq!(p["uoms"].as_array().unwrap().len(), 2);
    assert_eq!(p["uoms"][0]["isBase"], true);
    assert_eq!(p["uoms"][1]["factor"], "24");
    assert_eq!(p["uoms"][1]["baseEquivalent"], "24");
    assert_eq!(p["barcodes"][0]["barcode"], barcode);
    assert_eq!(p["customFields"][&fx.cf_key], 365);
    assert!(p["createdAt"].is_i64());
    let pid: i32 = c
        .app
        .scalar(&format!(
            "SELECT id FROM product WHERE uuid = '{}'",
            p["id"].as_str().unwrap()
        ))
        .await;
    assert!(audit_count(&c, "product", pid, 1).await >= 1);

    // konflik SKU / barcode, custom field tak dikenal, validasi.
    let r = c
        .app
        .post("/v1/admin/products")
        .token(admin)
        .json(&json!({ "sku": sku, "name": "x", "categoryId": fx.cat["id"] }))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(code_of(&r), "PRODUCT_SKU_TAKEN");
    let r = c
        .app
        .post("/v1/admin/products")
        .token(admin)
        .json(
            &json!({ "sku": format!("{sku}-2"), "name": "x", "categoryId": fx.cat["id"],
                       "barcodes": [{ "barcode": barcode }] }),
        )
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(
        r.json["message"],
        format!("Barcode {barcode} is already used by SKU {sku}")
    );
    let r = c
        .app
        .post("/v1/admin/products")
        .token(admin)
        .json(
            &json!({ "sku": format!("{sku}-3"), "name": "x", "categoryId": fx.cat["id"],
                       "customFields": { "nope": 1 } }),
        )
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "CUSTOM_FIELD_UNKNOWN_KEY");
    assert_eq!(r.json["details"]["issues"][0]["key"], "nope");
    let r = c
        .app
        .post("/v1/admin/products")
        .token(admin)
        .json(
            &json!({ "sku": "-bad", "name": "x", "categoryId": fx.cat["id"],
                       "uoms": [{ "uomId": fx.box_uom["id"], "conversionRate": "12" }] }),
        )
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec![
            "sku must be 1-100 letters, digits, dot, underscore, or hyphen",
            "uoms.0.conversionRate is no longer accepted — send operator and factor instead",
        ]
    );
    let r = c
        .app
        .post("/v1/admin/products")
        .token(admin)
        .json(
            &json!({ "sku": format!("{sku}-4"), "name": "x", "categoryId": uuid::Uuid::new_v4() }),
        )
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(code_of(&r), "PRODUCT_MASTER_NOT_FOUND");
    let r = c
        .app
        .get("/v1/admin/products")
        .token(&c.fx.staff.token)
        .send()
        .await;
    r.assert_status(401);

    // list: search barcode, filter kategori, cf filter + sort, summary.
    let r = c
        .app
        .get(&format!(
            "/v1/admin/products?search={barcode}&categoryId={}",
            fx.cat["id"].as_str().unwrap()
        ))
        .token(admin)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.json["meta"]["total"], 1);
    assert!(r.json["data"][0].get("uoms").is_none());
    let r = c
        .app
        .get(&format!(
            "/v1/admin/products?cf[{}]=365&sortBy=cf:{}&sortDir=desc",
            fx.cf_key, fx.cf_key
        ))
        .token(admin)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.json["meta"]["total"], 1);
    assert_eq!(r.json["data"][0]["sku"], sku);
    let r = c
        .app
        .get(&format!("/v1/admin/products?cf[{}]=abc", fx.cf_key))
        .token(admin)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(code_of(&r), "CUSTOM_FIELD_TYPE_MISMATCH");
    let r = c
        .app
        .get("/v1/admin/products?cf[zzz]=1")
        .token(admin)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(code_of(&r), "CUSTOM_FIELD_UNKNOWN_KEY");
    let r = c
        .app
        .get(&format!(
            "/v1/admin/products/custom-field-summary?key={}",
            fx.cf_key
        ))
        .token(admin)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.data()["values"], json!([{ "value": 365, "count": 1 }]));
    let r = c
        .app
        .get("/v1/admin/products/custom-field-summary?key=BAD")
        .token(admin)
        .send()
        .await;
    r.assert_status(400);

    // detail & update.
    let path = format!("/v1/admin/products/{}", p["id"].as_str().unwrap());
    let r = c.app.get(&path).token(&c.fx.manager.token).send().await;
    r.assert_ok();
    assert_eq!(r.data()["sku"], sku);
    let r = c
        .app
        .patch(&path)
        .token(admin)
        .json(
            &json!({ "name": "Coffee XYZ", "description": "d", "uoms": [],
                       "customFields": { fx.cf_key.clone(): null } }),
        )
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.data()["name"], "Coffee XYZ");
    assert_eq!(r.data()["uoms"].as_array().unwrap().len(), 1);
    assert_eq!(r.data()["customFields"], json!({}));
    let r = c
        .app
        .patch(&path)
        .token(admin)
        .json(&json!({ "uoms": [{ "uomId": fx.box_uom["id"], "isBase": true }] }))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "MULTIPLE_BASE_UOM");
    let r = c
        .app
        .get(&format!("/v1/admin/products/{}", uuid::Uuid::new_v4()))
        .token(admin)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(code_of(&r), "PRODUCT_NOT_FOUND");

    // export.
    let r = c
        .app
        .get(&format!(
            "/v1/admin/products/export?search={sku}&sortBy=sku"
        ))
        .token(admin)
        .send()
        .await;
    r.assert_ok();
    assert_eq!(
        r.headers.get("content-disposition").unwrap(),
        "attachment; filename=\"products-export.xlsx\""
    );
    let r = c
        .app
        .get("/v1/admin/products/export?sortBy=cf:x")
        .token(admin)
        .send()
        .await;
    r.assert_status(400);

    // delete: cascade + lepas barcode.
    let r = c.app.delete(&path).token(admin).send().await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(
        r.data(),
        &json!({ "id": p["id"], "deleted": true, "priceItemsDeleted": 0, "barcodesReleased": 1 })
    );
    let r = c.app.get(&path).token(admin).send().await;
    r.assert_status(404);
}

// ───────────────────────── gambar ─────────────────────────

#[tokio::test]
async fn product_image_flow() {
    let Some(c) = setup().await else { return };
    let fx = product_fixture(&c).await;
    let admin = &c.fx.admin.token;
    let r = c
        .app
        .post("/v1/admin/products")
        .token(admin)
        .json(
            &json!({ "sku": format!("IMG-{}", tag()), "name": "Img", "categoryId": fx.cat["id"] }),
        )
        .send()
        .await;
    r.assert_status(201);
    let pid = r.data()["id"].as_str().unwrap().to_owned();
    let base = format!("/v1/admin/products/{pid}/image");

    let r = c
        .app
        .post(&base)
        .token(admin)
        .json(&json!({ "contentType": "image/gif", "sizeBytes": 10 }))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "PRODUCT_IMAGE_TYPE_REJECTED");
    let r = c
        .app
        .post(&base)
        .token(admin)
        .json(&json!({ "contentType": "image/png", "sizeBytes": 6_000_000 }))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "PRODUCT_IMAGE_TOO_LARGE");
    let r = c.app.post(&base).token(admin).json(&json!({})).send().await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec![
            "contentType must be a string",
            "sizeBytes must not be less than 1",
            "sizeBytes must be an integer number"
        ]
    );
    let r = c
        .app
        .post(&base)
        .token(admin)
        .json(&json!({ "contentType": "image/png", "sizeBytes": 4 }))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    let key = r.data()["objectKey"].as_str().unwrap().to_owned();
    assert!(
        r.data()["uploadUrl"]
            .as_str()
            .unwrap()
            .contains("X-Goog-Signature")
    );
    assert_eq!(r.data()["maxSizeBytes"], 5_242_880);

    // confirm sebelum upload → 422; key produk lain → 422.
    let r = c
        .app
        .post(&format!("{base}/confirm"))
        .token(admin)
        .json(&json!({ "objectKey": key }))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "PRODUCT_IMAGE_NOT_UPLOADED");
    let r = c
        .app
        .post(&format!("{base}/confirm"))
        .token(admin)
        .json(&json!({ "objectKey": "products/1/x/y.png" }))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "PRODUCT_IMAGE_OBJECT_KEY_INVALID");

    c.em.put(&key, b"\x89PNG", "image/png");
    let r = c
        .app
        .post(&format!("{base}/confirm"))
        .token(admin)
        .json(&json!({ "objectKey": key }))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let url = r.data()["imageUrl"].as_str().unwrap().to_owned();
    assert!(url.starts_with(&format!("{HOST_BE}/products/{pid}/image/")));
    assert!(r.data()["imageUpdatedAt"].is_i64());

    // proxy publik tanpa login.
    let public = url.trim_start_matches(HOST_BE).to_owned();
    let r = c.app.get(&public).send().await;
    r.assert_ok();
    assert_eq!(
        r.headers.get("cache-control").unwrap(),
        "public, max-age=31536000, immutable"
    );
    let r = c
        .app
        .get(&format!("/products/{pid}/image/{}", uuid::Uuid::new_v4()))
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(code_of(&r), "PRODUCT_IMAGE_NOT_FOUND");
    let r = c
        .app
        .get(&format!("/products/{pid}/image/not-a-uuid"))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], "Validation failed (uuid is expected)");

    // hapus gambar: idempoten, object dihapus dari bucket.
    let r = c.app.delete(&base).token(admin).send().await;
    r.assert_ok();
    assert_eq!(r.data()["imageUrl"], Value::Null);
    assert!(c.em.deleted().contains(&key));
    let r = c.app.delete(&base).token(admin).send().await;
    r.assert_ok();
    let r = c.app.get(&public).send().await;
    r.assert_status(404);
}
