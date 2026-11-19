//! Uji integrasi 11 route mz-customers (customers 6 + customer masters 5) lewat HTTP terhadap Postgres
//! nyata: CRUD, validasi, konsistensi sub channel, kebijakan kolom (search/sort/sensor/export/reveal), audit.
//! Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-customers --test it_customers_flows`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::Duration;

use axum::Router;
use mz_core::AppState;
use mz_testkit::{Company, TestApp, TestResponse};
use serde_json::{Value, json};
use tokio::sync::{Mutex, MutexGuard};

static SERIAL: Mutex<()> = Mutex::const_new(());

fn build(state: &AppState) -> Router {
    mz_customers::router(state)
        .merge(mz_table_fields::router(state))
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

struct Ctx {
    app: TestApp,
    fx: Company,
    _guard: MutexGuard<'static, ()>,
}

async fn setup() -> Option<Ctx> {
    let guard = SERIAL.lock().await;
    let app = TestApp::new(build).await?;
    let fx = app.company().await;
    app.exec(&format!(
        r#"UPDATE company SET "moduleSalesTrackerEnabled" = true WHERE id = {cid};
           INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
             SELECT al.id, k.f, a.a::"PermissionAction" FROM "accessLevel" al
             CROSS JOIN (VALUES ('customers'), ('customer-masters'), ('stores')) AS k(f)
             CROSS JOIN (VALUES ('READ'), ('WRITE'), ('DELETE')) AS a(a)
             WHERE al."isSystem" ON CONFLICT DO NOTHING;
           DELETE FROM "tableFieldVisibility" WHERE "companyId" = {cid};
           INSERT INTO "customerType" ("companyId", code, name, "createdById")
             VALUES ({cid}, 'DISTRIBUTOR', 'Distributor', {uid}) ON CONFLICT DO NOTHING;
           UPDATE "customerType" SET "isActive" = true WHERE "companyId" = {cid} AND code = 'DISTRIBUTOR';"#,
        cid = fx.id,
        uid = fx.admin.id
    ))
    .await;
    mz_table_fields::policy::invalidate_company_local(fx.id);
    Some(Ctx {
        app,
        fx,
        _guard: guard,
    })
}

fn messages(r: &TestResponse) -> Vec<String> {
    r.json["message"]
        .as_array()
        .unwrap_or_else(|| panic!("message bukan array: {}", r.text()))
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

fn code_of(r: &TestResponse) -> String {
    r.json["code"].as_str().unwrap_or_default().to_owned()
}

fn assert_key_order(text: &str, keys: &[&str]) {
    let mut from = 0;
    for k in keys {
        let needle = format!("\"{k}\":");
        let pos = text[from..]
            .find(&needle)
            .unwrap_or_else(|| panic!("kunci `{k}` tidak ada sesudah {from} di {text}"));
        from += pos + needle.len();
    }
}

async fn audit_actions(c: &Ctx, entity: &str, uuid: &str, at_least: usize) -> Vec<String> {
    for _ in 0..50 {
        let rows: Vec<String> = sqlx::query_scalar(
            r#"SELECT action::text FROM "auditLog"
                WHERE "entityType"::text = $1 AND "entityUuid"::text = $2 ORDER BY id"#,
        )
        .bind(entity)
        .bind(uuid)
        .fetch_all(c.app.db())
        .await
        .unwrap();
        if rows.len() >= at_least {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("baris audit {entity}/{uuid} < {at_least}");
}

async fn new_master(c: &Ctx, kind: &str, body: Value) -> Value {
    let r = c
        .app
        .post(&format!("/v1/admin/customer-masters/{kind}"))
        .token(&c.fx.admin.token)
        .json(&body)
        .send()
        .await;
    r.assert_status(201);
    r.data().clone()
}

async fn new_customer(c: &Ctx, body: Value) -> Value {
    let r = c
        .app
        .post("/v1/admin/customers")
        .token(&c.fx.admin.token)
        .json(&body)
        .send()
        .await;
    r.assert_status(201);
    r.data().clone()
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

// ───────────────────────── customer masters ─────────────────────────

#[tokio::test]
async fn masters_crud_validation_and_audit() {
    let Some(c) = setup().await else { return };
    let app = &c.app;
    let base = "/v1/admin/customer-masters/types";

    // 401 tanpa token; role salah (STAFF) ditolak RolesGuard; MANAGER boleh.
    app.get(base).send().await.assert_status(401);
    app.get(base)
        .token(&c.fx.staff.token)
        .send()
        .await
        .assert_status(401);
    app.get(base)
        .token(&c.fx.manager.token)
        .send()
        .await
        .assert_status(200)
        .assert_no_sql_error();

    // `:kind` tidak dikenal → 400 SEBELUM body divalidasi.
    let r = app
        .post("/v1/admin/customer-masters/nope")
        .token(&c.fx.admin.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        "Validation failed (enum string is expected)"
    );

    // Validasi body.
    let r = app
        .post(base)
        .token(&c.fx.admin.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec![
            "code must be 1-32 characters of uppercase letters, digits, underscore, or hyphen",
            "code must be a string",
            "name must be longer than or equal to 1 characters",
            "name must be a string",
        ]
    );

    // Buat: kode di-uppercase; bentuk & urutan kunci.
    let r = app
        .post(base)
        .token(&c.fx.admin.token)
        .json(&json!({"code": "it-wholesale", "name": "IT Wholesale", "description": "catatan"}))
        .send()
        .await;
    r.assert_status(201);
    assert_key_order(
        &r.text(),
        &[
            "id",
            "code",
            "name",
            "description",
            "isActive",
            "createdAt",
            "updatedAt",
        ],
    );
    let t = r.data().clone();
    assert_eq!(t["code"], "IT-WHOLESALE");
    assert_eq!(t["isActive"], true);
    assert!(t["createdAt"].is_i64());
    assert!(t.get("channel").is_none());
    let uuid = id(&t);

    // 409 kode kembar (kode ber-bentuk objek `{code, message}`).
    let r = app
        .post(base)
        .token(&c.fx.admin.token)
        .json(&json!({"code": "IT-WHOLESALE", "name": "x"}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(code_of(&r), "CUSTOMER_MASTER_CODE_TAKEN");
    assert_eq!(
        r.json["message"],
        "Customer type code IT-WHOLESALE is already used in this company"
    );

    // Daftar: filter search + meta.
    let r = app
        .get(&format!("{base}?search=it-whole&limit=5"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(
        r.json["meta"],
        json!({"total":1,"page":1,"limit":5,"totalPages":1})
    );
    assert_eq!(r.data()[0]["id"], uuid);
    let r = app
        .get(&format!("{base}?limit=0"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(messages(&r), vec!["limit must not be less than 1"]);

    // Detail + 404 + uuid rusak (P2023 → 500) + lintas company.
    app.get(&format!("{base}/{uuid}"))
        .token(&c.fx.admin.token)
        .send()
        .await
        .assert_status(200);
    let r = app
        .get(&format!("{base}/00000000-0000-4000-8000-000000000000"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(code_of(&r), "CUSTOMER_MASTER_NOT_FOUND");
    assert_eq!(r.json["message"], "Customer type not found");
    app.get(&format!("{base}/not-a-uuid"))
        .token(&c.fx.admin.token)
        .send()
        .await
        .assert_status(500);
    let other = app.other_company_admin().await;
    app.get(&format!("{base}/{uuid}"))
        .token(&other.admin.token)
        .send()
        .await
        .assert_status(404);

    // Patch: hanya field yang dikirim; description null → NULL; kode kembar → 409.
    let r = app
        .patch(&format!("{base}/{uuid}"))
        .token(&c.fx.admin.token)
        .json(&json!({"name": "IT Wholesale 2", "description": null}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["name"], "IT Wholesale 2");
    assert_eq!(r.data()["description"], Value::Null);
    assert!(r.data()["updatedAt"].is_i64());
    new_master(&c, "types", json!({"code": "IT-OTHER", "name": "Other"})).await;
    let r = app
        .patch(&format!("{base}/{uuid}"))
        .token(&c.fx.admin.token)
        .json(&json!({"code": "it-other"}))
        .send()
        .await;
    r.assert_status(409);
    // `channelId` di kind selain sub-channels → 422.
    let r = app
        .patch(&format!("{base}/{uuid}"))
        .token(&c.fx.admin.token)
        .json(&json!({"channelId": "9f1c2d3e-4a5b-4c7d-8e9f-0a1b2c3d4e5f"}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "CUSTOMER_MASTER_INVALID_FIELD");
    // `name: null` → Prisma melempar → 500.
    app.patch(&format!("{base}/{uuid}"))
        .token(&c.fx.admin.token)
        .json(&json!({"name": null}))
        .send()
        .await
        .assert_status(500);

    // Hapus = arsip.
    let r = app
        .delete(&format!("{base}/{uuid}"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json, json!({"data": {"id": uuid, "archived": true}}));
    let r = app
        .get(&format!("{base}?isActive=false&search=IT-WHOLESALE"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    assert_eq!(r.json["meta"]["total"], 1);

    // Audit: create + update (+ update arsip).
    let acts = audit_actions(&c, "customerType", &uuid, 3).await;
    assert_eq!(acts[0], "create");
}

#[tokio::test]
async fn sub_channels_parent_rules_and_reparent_guard() {
    let Some(c) = setup().await else { return };
    let app = &c.app;
    let sub = "/v1/admin/customer-masters/sub-channels";

    let ch1 = new_master(
        &c,
        "channels",
        json!({"code": "IT-CH1", "name": "Channel 1"}),
    )
    .await;
    let ch2 = new_master(
        &c,
        "channels",
        json!({"code": "IT-CH2", "name": "Channel 2"}),
    )
    .await;

    // Tanpa channelId → 422; channel asing → 404.
    let r = app
        .post(sub)
        .token(&c.fx.admin.token)
        .json(&json!({"code": "IT-S0", "name": "S0"}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "SUBCHANNEL_WITHOUT_CHANNEL");
    let r = app
        .post(sub)
        .token(&c.fx.admin.token)
        .json(&json!({"code": "IT-S0", "name": "S0", "channelId": "9f1c2d3e-4a5b-4c7d-8e9f-0a1b2c3d4e5f"}))
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(r.json["message"], "Customer channel not found");

    // Ok: ada kunci `channel` (sesudah isActive).
    let r = app
        .post(sub)
        .token(&c.fx.admin.token)
        .json(&json!({"code": "IT-S1", "name": "Sub 1", "channelId": id(&ch1)}))
        .send()
        .await;
    r.assert_status(201);
    assert_key_order(
        &r.text(),
        &[
            "id",
            "code",
            "name",
            "description",
            "isActive",
            "channel",
            "createdAt",
        ],
    );
    assert_eq!(r.data()["channel"]["code"], "IT-CH1");
    let s1 = r.data().clone();

    // Channel diarsip → 422.
    let arch = new_master(
        &c,
        "channels",
        json!({"code": "IT-CH-ARCH", "name": "Arch"}),
    )
    .await;
    app.delete(&format!(
        "/v1/admin/customer-masters/channels/{}",
        id(&arch)
    ))
    .token(&c.fx.admin.token)
    .send()
    .await
    .assert_status(200);
    let r = app
        .post(sub)
        .token(&c.fx.admin.token)
        .json(&json!({"code": "IT-S2", "name": "S2", "channelId": id(&arch)}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "CUSTOMER_MASTER_ARCHIVED");

    // Daftar: filter ?channelId= dan kunci `channel` di tiap baris.
    let r = app
        .get(&format!("{sub}?channelId={}", id(&ch1)))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.data().as_array().unwrap().len(), 1);
    assert_eq!(r.data()[0]["channel"]["id"], ch1["id"]);

    // Customer memakai sub channel → memindah channel ditolak 409; mengirim ulang channel yang sama boleh.
    let cust = new_customer(
        &c,
        json!({"code": "IT-RP1", "name": "Reparent", "channelId": id(&ch1), "subChannelId": id(&s1)}),
    )
    .await;
    let r = app
        .patch(&format!("{sub}/{}", id(&s1)))
        .token(&c.fx.admin.token)
        .json(&json!({"channelId": id(&ch2)}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["error"], "SUBCHANNEL_IN_USE");
    assert_eq!(r.json["statusCode"], 409);
    assert_eq!(
        r.json["message"],
        "Sub channel is still used by 1 customer(s) under its current channel. Reassign them before moving it to another channel."
    );
    app.patch(&format!("{sub}/{}", id(&s1)))
        .token(&c.fx.admin.token)
        .json(&json!({"channelId": id(&ch1), "name": "Sub 1b"}))
        .send()
        .await
        .assert_status(200);
    // Customer dilepas → pemindahan boleh.
    app.delete(&format!("/v1/admin/customers/{}", id(&cust)))
        .token(&c.fx.admin.token)
        .send()
        .await
        .assert_status(200);
    let r = app
        .patch(&format!("{sub}/{}", id(&s1)))
        .token(&c.fx.admin.token)
        .json(&json!({"channelId": id(&ch2)}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["channel"]["code"], "IT-CH2");
    assert!(
        audit_actions(&c, "customerSubChannel", &id(&s1), 2)
            .await
            .len()
            >= 2
    );
}

// ───────────────────────── customers ─────────────────────────

#[tokio::test]
async fn customers_crud_defaults_and_consistency() {
    let Some(c) = setup().await else { return };
    let app = &c.app;
    let base = "/v1/admin/customers";

    app.get(base).send().await.assert_status(401);
    app.get(base)
        .token(&c.fx.staff.token)
        .send()
        .await
        .assert_status(401);

    // Validasi.
    let r = app
        .post(base)
        .token(&c.fx.admin.token)
        .json(&json!({"code": "bad code", "name": "", "typeId": "x", "isActive": "y"}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec![
            "code must be 1-32 characters of uppercase letters, digits, underscore, or hyphen",
            "name must be longer than or equal to 1 characters",
            "typeId must be a UUID",
            "isActive must be a boolean value",
        ]
    );

    // Buat minimal: tipe bawaan DISTRIBUTOR, kunci berurutan, telepon & NPWP dinormalisasi.
    let r = app
        .post(base)
        .token(&c.fx.admin.token)
        .json(&json!({"code": "it-c1", "name": "PT Sumber Makmur",
                      "taxNumber": " NPWP: 01.234.567.8-901.000 ", "phone": "0812-3456-7890"}))
        .send()
        .await;
    r.assert_status(201);
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
    let c1 = r.data().clone();
    assert_eq!(c1["code"], "IT-C1");
    assert_eq!(c1["type"]["code"], "DISTRIBUTOR");
    assert_eq!(c1["group"], Value::Null);
    assert_eq!(c1["taxNumber"], "01.234.567.8-901.000");
    assert_eq!(c1["phone"], "+6281234567890");
    assert_eq!(c1["isActive"], true);
    assert_eq!(c1["storeCount"], 0);
    assert!(c1["updatedAt"].is_i64() || c1["updatedAt"].is_null());
    let c1_id = id(&c1);

    // Kode kembar → 409 {code, message}; sudah dipakai ulang setelah soft delete (dicek di bawah).
    let r = app
        .post(base)
        .token(&c.fx.admin.token)
        .json(&json!({"code": "IT-C1", "name": "dup"}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(code_of(&r), "CUSTOMER_CODE_TAKEN");
    assert_eq!(
        r.json["message"],
        "Customer code IT-C1 is already used in this company"
    );

    // Master: 404 / 422 arsip / price list 404.
    let r = app
        .post(base)
        .token(&c.fx.admin.token)
        .json(&json!({"code": "IT-C2", "name": "x", "groupId": "9f1c2d3e-4a5b-4c7d-8e9f-0a1b2c3d4e5f"}))
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(code_of(&r), "CUSTOMER_MASTER_NOT_FOUND");
    let arch = new_master(&c, "groups", json!({"code": "IT-G-ARCH", "name": "Arch"})).await;
    app.delete(&format!("/v1/admin/customer-masters/groups/{}", id(&arch)))
        .token(&c.fx.admin.token)
        .send()
        .await
        .assert_status(200);
    let r = app
        .post(base)
        .token(&c.fx.admin.token)
        .json(&json!({"code": "IT-C2", "name": "x", "groupId": id(&arch)}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "CUSTOMER_MASTER_ARCHIVED");
    let r = app
        .post(base)
        .token(&c.fx.admin.token)
        .json(&json!({"code": "IT-C2", "name": "x", "priceListId": "9f1c2d3e-4a5b-4c7d-8e9f-0a1b2c3d4e5f"}))
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(code_of(&r), "PRICE_LIST_NOT_FOUND");

    // Konsistensi channel/sub channel (§5.7), termasuk 3b pada PATCH.
    let ch1 = new_master(&c, "channels", json!({"code": "IT-X-CH1", "name": "CH1"})).await;
    let ch2 = new_master(&c, "channels", json!({"code": "IT-X-CH2", "name": "CH2"})).await;
    let sub = new_master(
        &c,
        "sub-channels",
        json!({"code": "IT-X-SUB", "name": "SUB", "channelId": id(&ch1)}),
    )
    .await;
    let r = app
        .post(base)
        .token(&c.fx.admin.token)
        .json(&json!({"code": "IT-C3", "name": "x", "subChannelId": id(&sub)}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "SUBCHANNEL_WITHOUT_CHANNEL");
    let r = app
        .post(base)
        .token(&c.fx.admin.token)
        .json(
            &json!({"code": "IT-C3", "name": "x", "channelId": id(&ch2), "subChannelId": id(&sub)}),
        )
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "SUBCHANNEL_CHANNEL_MISMATCH");
    let c3 = new_customer(
        &c,
        json!({"code": "IT-C3", "name": "Full", "channelId": id(&ch1), "subChannelId": id(&sub)}),
    )
    .await;
    assert_eq!(c3["subChannel"]["code"], "IT-X-SUB");
    // 3b: ganti channel tanpa sub channel → mismatch; channel null padahal sub channel masih ada → without channel.
    let r = app
        .patch(&format!("{base}/{}", id(&c3)))
        .token(&c.fx.admin.token)
        .json(&json!({"channelId": id(&ch2)}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "SUBCHANNEL_CHANNEL_MISMATCH");
    let r = app
        .patch(&format!("{base}/{}", id(&c3)))
        .token(&c.fx.admin.token)
        .json(&json!({"channelId": null}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(code_of(&r), "SUBCHANNEL_WITHOUT_CHANNEL");
    // Lepas keduanya sekaligus → boleh.
    let r = app
        .patch(&format!("{base}/{}", id(&c3)))
        .token(&c.fx.admin.token)
        .json(&json!({"channelId": null, "subChannelId": null}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["channel"], Value::Null);

    // Patch: typeId null melepas tipe; yang tak dikirim tidak berubah; NPWP/telepon dinormalisasi.
    let r = app
        .patch(&format!("{base}/{c1_id}"))
        .token(&c.fx.admin.token)
        .json(&json!({"typeId": null, "taxNumber": "NPWP 09.876", "phone": "6281"}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["type"], Value::Null);
    assert_eq!(r.data()["taxNumber"], "09.876");
    assert_eq!(r.data()["phone"], "+6281");
    assert_eq!(r.data()["name"], "PT Sumber Makmur");
    assert!(r.data()["updatedAt"].is_i64());
    // `name: null` → 500; kode kembar via PATCH → 409; 404; uuid rusak → 500; lintas company → 404.
    app.patch(&format!("{base}/{c1_id}"))
        .token(&c.fx.admin.token)
        .json(&json!({"name": null}))
        .send()
        .await
        .assert_status(500);
    let r = app
        .patch(&format!("{base}/{c1_id}"))
        .token(&c.fx.admin.token)
        .json(&json!({"code": "it-c3"}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(
        r.json["message"],
        "Customer code IT-C3 is already used in this company"
    );
    let r = app
        .patch("/v1/admin/customers/00000000-0000-4000-8000-000000000000")
        .token(&c.fx.admin.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(code_of(&r), "CUSTOMER_NOT_FOUND");
    app.get("/v1/admin/customers/not-a-uuid")
        .token(&c.fx.admin.token)
        .send()
        .await
        .assert_status(500);
    let other = app.other_company_admin().await;
    app.get(&format!("{base}/{c1_id}"))
        .token(&other.admin.token)
        .send()
        .await
        .assert_status(404);

    // Hapus: ditolak selama punya toko aktif; sesudah toko dihapus lolos; kode bisa dipakai ulang.
    let cid_internal: i32 = app
        .scalar(&format!("SELECT id FROM customer WHERE uuid = '{c1_id}'"))
        .await;
    app.exec(&format!(
        r#"INSERT INTO store ("companyId", code, name, "ownerName", address, lat, lng, "createdById", "customerId")
           VALUES ({cid}, 'IT-ST-1', 'Toko IT', 'Owner', 'Jl. A', 0, 0, {uid}, {cust})"#,
        cid = c.fx.id,
        uid = c.fx.admin.id,
        cust = cid_internal
    ))
    .await;
    let r = app
        .get(&format!("{base}/{c1_id}"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    assert_eq!(r.data()["storeCount"], 1);
    let r = app
        .delete(&format!("{base}/{c1_id}"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["error"], "CUSTOMER_HAS_STORES");
    assert_eq!(
        r.json["message"],
        "Customer still has 1 stores. Reassign or remove them first."
    );
    app.exec(r#"UPDATE store SET "deletedAt" = NOW() WHERE code = 'IT-ST-1'"#)
        .await;
    let r = app
        .delete(&format!("{base}/{c1_id}"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json, json!({"data": {"id": c1_id, "deleted": true}}));
    app.get(&format!("{base}/{c1_id}"))
        .token(&c.fx.admin.token)
        .send()
        .await
        .assert_status(404);
    new_customer(&c, json!({"code": "IT-C1", "name": "Reuse"})).await;

    // Audit: create, update, update (soft delete).
    assert!(audit_actions(&c, "customer", &c1_id, 3).await.len() >= 3);
    app.exec("DELETE FROM store WHERE code = 'IT-ST-1'").await;
}

#[tokio::test]
async fn customers_list_filters_sort_and_export() {
    let Some(c) = setup().await else { return };
    let app = &c.app;
    let base = "/v1/admin/customers";

    let ch = new_master(&c, "channels", json!({"code": "IT-L-CH", "name": "LCH"})).await;
    let grp = new_master(&c, "groups", json!({"code": "IT-L-GRP", "name": "LGRP"})).await;
    let a = new_customer(
        &c,
        json!({"code": "IT-LA", "name": "Alpha ITL", "channelId": id(&ch), "groupId": id(&grp), "taxNumber": "11.111"}),
    )
    .await;
    let b = new_customer(
        &c,
        json!({"code": "IT-LB", "name": "Beta ITL", "isActive": false}),
    )
    .await;
    let _z = new_customer(&c, json!({"code": "IT-LC", "name": "Gamma ITL"})).await;
    let a_internal: i32 = app
        .scalar(&format!(
            "SELECT id FROM customer WHERE uuid = '{}'",
            id(&a)
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO store ("companyId", code, name, "ownerName", address, lat, lng, "createdById", "customerId")
           VALUES ({}, 'IT-ST-L', 'Toko L', 'Owner', 'Jl. A', 0, 0, {}, {a_internal})"#,
        c.fx.id, c.fx.admin.id
    ))
    .await;

    let list = |q: String| {
        let url = format!("{base}?{q}");
        let (app, token) = (&c.app, c.fx.admin.token.clone());
        async move {
            let r = app.get(&url).token(&token).send().await;
            r.assert_status(200).assert_no_sql_error();
            r
        }
    };
    let codes = |r: &TestResponse| -> Vec<String> {
        r.data()
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["code"].as_str().unwrap().to_owned())
            .collect()
    };

    let r = list("search=ITL&sortBy=code&sortDir=desc".into()).await;
    assert_eq!(codes(&r), vec!["IT-LC", "IT-LB", "IT-LA"]);
    assert_eq!(
        r.json["meta"],
        json!({"total":3,"page":1,"limit":20,"totalPages":1})
    );
    // Default urut nama asc.
    assert_eq!(
        codes(&list("search=ITL".into()).await),
        vec!["IT-LA", "IT-LB", "IT-LC"]
    );
    // search juga mengenai taxNumber.
    assert_eq!(codes(&list("search=11.111".into()).await), vec!["IT-LA"]);
    // isActive / hasStores (true dan false bermakna).
    assert_eq!(
        codes(&list("search=ITL&isActive=false".into()).await),
        vec!["IT-LB"]
    );
    assert_eq!(
        codes(&list("search=ITL&hasStores=true".into()).await),
        vec!["IT-LA"]
    );
    assert_eq!(
        codes(&list("search=ITL&hasStores=false".into()).await),
        vec!["IT-LB", "IT-LC"]
    );
    // Filter uuid: satu, berulang, dipisah koma, dan uuid asing = tidak ada baris.
    assert_eq!(
        codes(&list(format!("search=ITL&channelId={}", id(&ch))).await),
        vec!["IT-LA"]
    );
    assert_eq!(
        codes(&list(format!("search=ITL&groupId[]={}", id(&grp))).await),
        vec!["IT-LA"]
    );
    assert!(
        codes(&list("search=ITL&typeId=9f1c2d3e-4a5b-4c7d-8e9f-0a1b2c3d4e5f".into()).await)
            .is_empty()
    );
    assert_eq!(
        codes(&list(format!("search=ITL&channelId={},{}", id(&ch), id(&grp))).await).len(),
        1
    );
    // Paginasi.
    let r = list("search=ITL&limit=2&page=2".into()).await;
    assert_eq!(codes(&r), vec!["IT-LC"]);
    assert_eq!(r.json["meta"]["totalPages"], 2);
    // Validasi query.
    let r = app
        .get(&format!("{base}?typeId=x&sortBy=id&isActive=maybe"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec![
            "each value in typeId must be a UUID",
            "isActive must be a boolean value",
            "sortBy must be one of the following values: name, code, createdAt",
        ]
    );

    // Export: xlsx, filter yang sama, header file persis.
    let r = app
        .get(&format!("{base}/export?search=ITL&hasStores=false"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.headers.get("content-type").unwrap(),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    );
    assert_eq!(
        r.headers.get("content-disposition").unwrap(),
        "attachment; filename=\"customers-export.xlsx\""
    );
    assert_eq!(
        r.headers.get("content-length").unwrap().to_str().unwrap(),
        r.bytes.len().to_string()
    );
    assert_eq!(&r.bytes[..2], b"PK");
    // `/export` statis menang atas `/{id}`; query tak sah ditolak (page/limit tidak dikenal = diabaikan).
    app.get(&format!("{base}/export?isActive=zzz"))
        .token(&c.fx.admin.token)
        .send()
        .await
        .assert_status(400);
    app.get(&format!("{base}/export?page=zzz"))
        .token(&c.fx.admin.token)
        .send()
        .await
        .assert_status(200);
    let _ = b;
    app.exec("DELETE FROM store WHERE code = 'IT-ST-L'").await;
}

// ───────────────────────── kebijakan kolom ─────────────────────────

#[tokio::test]
async fn field_policy_censors_search_sort_export_and_reveal() {
    use std::io::Read;
    let Some(c) = setup().await else { return };
    let app = &c.app;
    let base = "/v1/admin/customers";
    let cid = c.fx.id;

    let cust = new_customer(
        &c,
        json!({"code": "IT-FP1", "name": "Policy Co", "taxNumber": "55.555.555", "phone": "0811999"}),
    )
    .await;
    let uuid = id(&cust);

    let set_policy = |col: &'static str, mode: &'static str| async move {
        app.exec(&format!(
            r#"DELETE FROM "tableFieldVisibility" WHERE "companyId" = {cid};
                   INSERT INTO "tableFieldVisibility" ("companyId", "tableKey", "columnKey", mode)
                   VALUES ({cid}, 'customers', '{col}', '{mode}'::"FieldVisibilityMode")"#
        ))
        .await;
        mz_table_fields::policy::invalidate_company_local(cid);
    };

    // masked: nilai null di list/detail/create/update + envelope fieldPolicy; search tidak mengenai taxNumber.
    set_policy("taxNumber", "masked").await;
    let r = app
        .get(&format!("{base}?search=IT-FP1"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()[0]["taxNumber"], Value::Null);
    assert_eq!(r.data()[0]["phone"], "+62811999");
    assert_eq!(r.json["fieldPolicy"]["tableKey"], "customers");
    assert_eq!(r.json["fieldPolicy"]["masked"], json!(["taxNumber"]));
    assert!(
        app.get(&format!("{base}?search=55.555"))
            .token(&c.fx.admin.token)
            .send()
            .await
            .data()
            .as_array()
            .unwrap()
            .is_empty()
    );
    let r = app
        .get(&format!("{base}/{uuid}"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    assert_eq!(r.data()["taxNumber"], Value::Null);
    assert_eq!(r.json["fieldPolicy"]["masked"], json!(["taxNumber"]));
    let r = app
        .patch(&format!("{base}/{uuid}"))
        .token(&c.fx.admin.token)
        .json(&json!({"name": "Policy Co 2"}))
        .send()
        .await;
    assert_eq!(r.data()["taxNumber"], Value::Null);
    assert!(r.json.get("fieldPolicy").is_some());
    // DELETE bukan baris customer: tanpa fieldPolicy (diuji di bawah dengan customer lain).

    // Export: kolom `masked` tetap berjudul tetapi nilainya kosong; `hidden` membuang kolom.
    let sheet = |bytes: &[u8]| {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut s = String::new();
        zip.by_name("xl/sharedStrings.xml")
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        s
    };
    let r = app
        .get(&format!("{base}/export?search=IT-FP1"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    let t = sheet(&r.bytes);
    assert!(t.contains(">taxNumber<") && !t.contains("55.555.555"));
    set_policy("taxNumber", "hidden").await;
    let r = app
        .get(&format!("{base}/export?search=IT-FP1"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    let t = sheet(&r.bytes);
    assert!(!t.contains(">taxNumber<") && !t.contains("55.555.555") && t.contains(">phone<"));

    // sort ke kolom tersensor → 400 FIELD_NOT_SORTABLE (sebelum query); kolom lain tetap boleh.
    set_policy("createdAt", "hidden").await;
    let r = app
        .get(&format!("{base}?sortBy=createdAt"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(code_of(&r), "FIELD_NOT_SORTABLE");
    assert_eq!(r.json["details"]["field"], "createdAt");
    app.get(&format!("{base}?sortBy=code"))
        .token(&c.fx.admin.token)
        .send()
        .await
        .assert_status(200);

    // revealable: POST /v1/admin/table-fields/reveal memakai resolver customers → nilai asli + jejak log.
    set_policy("taxNumber", "revealable").await;
    let r = app
        .post("/v1/admin/table-fields/reveal")
        .token(&c.fx.admin.token)
        .json(&json!({"tableKey": "customers", "recordId": uuid, "field": "taxNumber", "reason": "audit"}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["field"], "taxNumber");
    assert_eq!(r.data()["value"], "55.555.555");
    let label: String = app
        .scalar(&format!(
            r#"SELECT "recordLabel" FROM "fieldRevealLog" WHERE "companyId" = {cid} AND "tableKey" = 'customers'
               ORDER BY id DESC LIMIT 1"#
        ))
        .await;
    assert_eq!(label, "IT-FP1 \u{2014} Policy Co 2");
    // record asing → 404 CUSTOMER_NOT_FOUND milik customers.
    let r = app
        .post("/v1/admin/table-fields/reveal")
        .token(&c.fx.admin.token)
        .json(&json!({"tableKey": "customers", "recordId": "00000000-0000-4000-8000-000000000000", "field": "taxNumber"}))
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(code_of(&r), "CUSTOMER_NOT_FOUND");

    // DELETE tanpa fieldPolicy meski ada kebijakan.
    let r = app
        .delete(&format!("{base}/{uuid}"))
        .token(&c.fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert!(r.json.get("fieldPolicy").is_none());

    app.exec(&format!(
        r#"DELETE FROM "tableFieldVisibility" WHERE "companyId" = {cid}"#
    ))
    .await;
    mz_table_fields::policy::invalidate_company_local(cid);
}
