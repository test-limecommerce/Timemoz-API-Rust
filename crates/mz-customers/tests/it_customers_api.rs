//! Uji integrasi 11 route mz-customers (admin customers 6 + customer masters 5) lewat HTTP terhadap
//! Postgres nyata, termasuk audit, kebijakan kolom (`tableFieldVisibility`), export xlsx, dan reveal.
//! Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-customers --test it_customers_api`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::io::Read;
use std::time::Duration;

use axum::Router;
use mz_core::AppState;
use mz_testkit::{Company, TestApp, TestResponse};
use serde_json::{Value, json};
use tokio::sync::{Mutex, MutexGuard};

static SERIAL: Mutex<()> = Mutex::const_new(());

const M: &str = "/v1/admin/customer-masters";
const C: &str = "/v1/admin/customers";
const CODE_MSG: &str =
    "code must be 1-32 characters of uppercase letters, digits, underscore, or hyphen";

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
             CROSS JOIN (VALUES ('customers'), ('customer-masters'), ('stores'), ('table-fields')) AS k(f)
             CROSS JOIN (VALUES ('READ'), ('WRITE'), ('DELETE')) AS a(a)
             WHERE al."isSystem" ON CONFLICT DO NOTHING;
           DELETE FROM "tableFieldVisibility" WHERE "companyId" = {cid};
           DELETE FROM "fieldRevealLog" WHERE "companyId" = {cid};
           DELETE FROM "fieldRevealQuota" WHERE "companyId" = {cid};
           UPDATE store SET "customerId" = NULL WHERE "companyId" = {cid};
           DELETE FROM customer WHERE "companyId" = {cid};
           DELETE FROM "customerSubChannel" WHERE "companyId" = {cid};
           DELETE FROM "customerChannel" WHERE "companyId" = {cid};
           DELETE FROM "customerGroup" WHERE "companyId" = {cid};
           DELETE FROM "customerType" WHERE "companyId" = {cid};
           DELETE FROM "priceList" WHERE "companyId" = {cid};"#,
        cid = fx.id
    ))
    .await;
    mz_table_fields::policy::invalidate_company_local(fx.id);
    Some(Ctx {
        app,
        fx,
        _guard: guard,
    })
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

fn messages(r: &TestResponse) -> Vec<String> {
    r.json["message"]
        .as_array()
        .unwrap_or_else(|| panic!("message bukan array: {}", r.text()))
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

fn v4() -> String {
    uuid::Uuid::new_v4().to_string()
}

async fn audit_actions(c: &Ctx, entity: &str, id: i32, at_least: usize) -> Vec<String> {
    for _ in 0..50 {
        let rows: Vec<String> = sqlx::query_scalar(
            r#"SELECT action::text FROM "auditLog"
                WHERE "entityType"::text = $1 AND "entityId" = $2 ORDER BY id"#,
        )
        .bind(entity)
        .bind(id)
        .fetch_all(c.app.db())
        .await
        .unwrap();
        if rows.len() >= at_least {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("baris audit {entity}#{id} < {at_least}");
}

async fn new_master(c: &Ctx, kind: &str, body: Value) -> Value {
    let r = c
        .app
        .post(&format!("{M}/{kind}"))
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
        .post(C)
        .token(&c.fx.admin.token)
        .json(&body)
        .send()
        .await;
    r.assert_status(201);
    r.data().clone()
}

async fn internal_id(c: &Ctx, table: &str, uuid: &str) -> i32 {
    c.app
        .scalar(&format!(
            r#"SELECT id FROM "{table}" WHERE uuid = '{uuid}'"#
        ))
        .await
}

/// Isi sharedStrings.xml sebuah xlsx.
fn shared_strings(bytes: &[u8]) -> String {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut out = String::new();
    zip.by_name("xl/sharedStrings.xml")
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    out
}

// ───────────────────────── customer masters ─────────────────────────

#[tokio::test]
async fn masters_crud_validation_in_use_and_audit() {
    let Some(c) = setup().await else { return };
    let app = &c.app;
    let admin = &c.fx.admin.token;

    // Gerbang: tanpa token 401; STAFF ditolak RolesGuard (401, pesan Nest).
    app.get(&format!("{M}/types"))
        .send()
        .await
        .assert_status(401);
    let r = app
        .get(&format!("{M}/types"))
        .token(&c.fx.staff.token)
        .send()
        .await;
    r.assert_status(401);
    assert_eq!(r.json["message"], "Access denied: insufficient permissions");
    // MANAGER boleh.
    app.get(&format!("{M}/types"))
        .token(&c.fx.manager.token)
        .send()
        .await
        .assert_status(200);

    // `:kind` tidak dikenal → 400 SEBELUM validasi body.
    for (method, body) in [("GET", json!({})), ("POST", json!({}))] {
        let rb = if method == "GET" {
            app.get(&format!("{M}/nope"))
        } else {
            app.post(&format!("{M}/nope"))
        };
        let r = rb.token(admin).json(&body).send().await;
        r.assert_status(400);
        assert_eq!(
            r.json["message"],
            "Validation failed (enum string is expected)"
        );
    }

    // Validasi create.
    let r = app
        .post(&format!("{M}/types"))
        .token(admin)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec![
            CODE_MSG,
            "code must be a string",
            "name must be longer than or equal to 1 characters",
            "name must be a string"
        ]
    );
    let r = app
        .post(&format!("{M}/types"))
        .token(admin)
        .json(&json!({"code":"A","name":"n","description":"x".repeat(256),"channelId":"zz"}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec![
            "description must be shorter than or equal to 255 characters",
            "channelId must be a UUID"
        ]
    );

    // Create: kode di-uppercase; bentuk & urutan kunci.
    let r = app
        .post(&format!("{M}/types"))
        .token(admin)
        .json(&json!({"code":"distributor","name":"Distributor","description":"Bawaan"}))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
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
    let ty = r.data().clone();
    assert_eq!(ty["code"], "DISTRIBUTOR");
    assert_eq!(ty["description"], "Bawaan");
    assert_eq!(ty["isActive"], true);
    assert!(ty["createdAt"].is_i64() && ty.get("channel").is_none());
    let ty_uuid = ty["id"].as_str().unwrap().to_owned();
    // Tanpa description → null.
    let g = new_master(&c, "groups", json!({"code":"vip","name":"VIP"})).await;
    assert_eq!(g["description"], Value::Null);

    // Kode kembar → 409 (per tabel, per company).
    let r = app
        .post(&format!("{M}/types"))
        .token(admin)
        .json(&json!({"code":"DISTRIBUTOR","name":"Lain"}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(
        r.json,
        json!({"code":"CUSTOMER_MASTER_CODE_TAKEN","message":"Customer type code DISTRIBUTOR is already used in this company"})
    );
    // Kode sama di tabel lain boleh.
    new_master(&c, "channels", json!({"code":"DISTRIBUTOR","name":"x"})).await;
    // Company lain boleh memakai kode yang sama.
    let other = app.other_company_admin().await;
    app.post(&format!("{M}/types"))
        .token(&other.admin.token)
        .json(&json!({"code":"DISTRIBUTOR","name":"Beda company"}))
        .send()
        .await
        .assert_status(201);

    // channelId hanya untuk sub-channels.
    let r = app
        .post(&format!("{M}/types"))
        .token(admin)
        .json(&json!({"code":"Z","name":"z","channelId":v4()}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(
        r.json,
        json!({"code":"CUSTOMER_MASTER_INVALID_FIELD","message":"channelId is only accepted for Customer sub channel"})
    );

    // Sub channel: butuh channel aktif milik company ini.
    let ch = new_master(&c, "channels", json!({"code":"GT","name":"General Trade"})).await;
    let ch_uuid = ch["id"].as_str().unwrap().to_owned();
    let r = app
        .post(&format!("{M}/sub-channels"))
        .token(admin)
        .json(&json!({"code":"WRG","name":"Warung"}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(r.json["code"], "SUBCHANNEL_WITHOUT_CHANNEL");
    assert_eq!(
        r.json["message"],
        "channelId is required to create a sub channel"
    );
    let r = app
        .post(&format!("{M}/sub-channels"))
        .token(admin)
        .json(&json!({"code":"WRG","name":"Warung","channelId":v4()}))
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(
        r.json,
        json!({"code":"CUSTOMER_MASTER_NOT_FOUND","message":"Customer channel not found"})
    );
    let r = app
        .post(&format!("{M}/sub-channels"))
        .token(admin)
        .json(&json!({"code":"WRG","name":"Warung","channelId":ch_uuid}))
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
            "updatedAt",
        ],
    );
    let sub = r.data().clone();
    assert_eq!(
        sub["channel"],
        json!({"id": ch_uuid, "code": "GT", "name": "General Trade"})
    );
    let sub_uuid = sub["id"].as_str().unwrap().to_owned();

    // Channel diarsip tidak bisa dipasang.
    let ch2 = new_master(&c, "channels", json!({"code":"MT","name":"Modern Trade"})).await;
    let ch2_uuid = ch2["id"].as_str().unwrap().to_owned();
    app.delete(&format!("{M}/channels/{ch2_uuid}"))
        .token(admin)
        .send()
        .await
        .assert_status(200);
    let r = app
        .post(&format!("{M}/sub-channels"))
        .token(admin)
        .json(&json!({"code":"SM","name":"Supermarket","channelId":ch2_uuid}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(
        r.json,
        json!({"code":"CUSTOMER_MASTER_ARCHIVED","message":"Cannot assign an archived customer channel"})
    );

    // List: pagination, search (code/name/description), isActive, filter channel (sub-channels).
    let r = app
        .get(&format!("{M}/types?search=bawaan&page=1&limit=5"))
        .token(admin)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["data"].as_array().unwrap().len(), 1);
    assert_eq!(
        r.json["meta"],
        json!({"total":1,"page":1,"limit":5,"totalPages":1})
    );
    let r = app
        .get(&format!("{M}/channels?isActive=false"))
        .token(admin)
        .send()
        .await;
    assert_eq!(r.json["data"].as_array().unwrap().len(), 1);
    assert_eq!(r.json["data"][0]["code"], "MT");
    let r = app
        .get(&format!("{M}/channels?limit=1&page=2"))
        .token(admin)
        .send()
        .await;
    assert_eq!(r.json["meta"]["total"], 3);
    assert_eq!(r.json["meta"]["totalPages"], 3);
    let r = app
        .get(&format!("{M}/sub-channels?channelId={ch_uuid}"))
        .token(admin)
        .send()
        .await;
    assert_eq!(r.json["data"][0]["channel"]["code"], "GT");
    // Filter channel diarsip → 422 (sama dengan Node).
    app.get(&format!("{M}/sub-channels?channelId={ch2_uuid}"))
        .token(admin)
        .send()
        .await
        .assert_status(422);
    let r = app
        .get(&format!("{M}/types?limit=101&isActive=maybe&channelId=x"))
        .token(admin)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec![
            "isActive must be a boolean value",
            "channelId must be a UUID",
            "limit must not be greater than 100"
        ]
    );
    // Urut name ASC lalu id.
    let r = app.get(&format!("{M}/channels")).token(admin).send().await;
    let names: Vec<&str> = r.json["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["General Trade", "Modern Trade", "x"]);

    // Detail: 200 / 404 / uuid rusak (P2023 → 500) / lintas company 404.
    let r = app
        .get(&format!("{M}/sub-channels/{sub_uuid}"))
        .token(admin)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["channel"]["id"], ch_uuid);
    let r = app
        .get(&format!("{M}/sub-channels/{}", v4()))
        .token(admin)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(
        r.json,
        json!({"code":"CUSTOMER_MASTER_NOT_FOUND","message":"Customer sub channel not found"})
    );
    app.get(&format!("{M}/types/not-a-uuid"))
        .token(admin)
        .send()
        .await
        .assert_status(500);
    app.get(&format!("{M}/types/{ty_uuid}"))
        .token(&other.admin.token)
        .send()
        .await
        .assert_status(404);
    // Satu uuid, kind lain → 404 (tabel terpisah).
    app.get(&format!("{M}/groups/{ty_uuid}"))
        .token(admin)
        .send()
        .await
        .assert_status(404);

    // Update: hanya field yang dikirim; isActive boleh; null pada kolom non-null → 500.
    let r = app
        .patch(&format!("{M}/types/{ty_uuid}"))
        .token(admin)
        .json(&json!({"name":"Distributor Besar","description":null}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["name"], "Distributor Besar");
    assert_eq!(r.data()["description"], Value::Null);
    assert_eq!(r.data()["code"], "DISTRIBUTOR");
    for body in [
        json!({"name":null}),
        json!({"isActive":null}),
        json!({"code":null}),
    ] {
        app.patch(&format!("{M}/types/{ty_uuid}"))
            .token(admin)
            .json(&body)
            .send()
            .await
            .assert_status(500);
    }
    let r = app
        .patch(&format!("{M}/types/{ty_uuid}"))
        .token(admin)
        .json(&json!({"isActive":"x","code":"bad code"}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec![CODE_MSG, "isActive must be a boolean value"]
    );
    // Kode baru bentrok → 409 dengan kode yang dikirim.
    let w = new_master(&c, "types", json!({"code":"WHOLESALER","name":"W"})).await;
    let r = app
        .patch(&format!("{M}/types/{}", w["id"].as_str().unwrap()))
        .token(admin)
        .json(&json!({"code":"distributor"}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "CUSTOMER_MASTER_CODE_TAKEN");
    assert_eq!(
        r.json["message"],
        "Customer type code DISTRIBUTOR is already used in this company"
    );
    // channelId untuk kind lain → 422; row tak ada → 404.
    app.patch(&format!("{M}/types/{ty_uuid}"))
        .token(admin)
        .json(&json!({"channelId": v4()}))
        .send()
        .await
        .assert_status(422);
    app.patch(&format!("{M}/types/{}", v4()))
        .token(admin)
        .json(&json!({"name":"x"}))
        .send()
        .await
        .assert_status(404);

    // Pindah sub channel: ditolak selama customer aktif masih memakainya (409 SUBCHANNEL_IN_USE).
    let sub_id = internal_id(&c, "customerSubChannel", &sub_uuid).await;
    let ch_id = internal_id(&c, "customerChannel", &ch_uuid).await;
    let ty_id = internal_id(&c, "customerType", &ty_uuid).await;
    app.exec(&format!(
        r#"INSERT INTO customer ("companyId", code, name, "typeId", "channelId", "subChannelId", "createdById")
           VALUES ({}, 'CUST-IU', 'In Use', {ty_id}, {ch_id}, {sub_id}, {})"#,
        c.fx.id, c.fx.admin.id
    ))
    .await;
    let ch3 = new_master(&c, "channels", json!({"code":"HR","name":"Horeca"})).await;
    let ch3_uuid = ch3["id"].as_str().unwrap().to_owned();
    let r = app
        .patch(&format!("{M}/sub-channels/{sub_uuid}"))
        .token(admin)
        .json(&json!({"channelId": ch3_uuid}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(
        r.json,
        json!({"statusCode":409,"error":"SUBCHANNEL_IN_USE","message":"Sub channel is still used by 1 customer(s) under its current channel. Reassign them before moving it to another channel."})
    );
    // Mengirim ulang channel yang sama bukan pemindahan.
    app.patch(&format!("{M}/sub-channels/{sub_uuid}"))
        .token(admin)
        .json(&json!({"channelId": ch_uuid, "name": "Warung 2"}))
        .send()
        .await
        .assert_status(200);
    // Channel arsip sebagai tujuan → 422.
    app.patch(&format!("{M}/sub-channels/{sub_uuid}"))
        .token(admin)
        .json(&json!({"channelId": ch2_uuid}))
        .send()
        .await
        .assert_status(422);
    // `channelId: null` pada sub-channels → Prisma menolak → 500.
    app.patch(&format!("{M}/sub-channels/{sub_uuid}"))
        .token(admin)
        .json(&json!({"channelId": null}))
        .send()
        .await
        .assert_status(500);
    // Tak dipakai lagi → boleh pindah, `channel` di respons ikut berubah.
    app.exec(
        "UPDATE customer SET \"subChannelId\" = NULL, \"channelId\" = NULL WHERE code = 'CUST-IU'",
    )
    .await;
    let r = app
        .patch(&format!("{M}/sub-channels/{sub_uuid}"))
        .token(admin)
        .json(&json!({"channelId": ch3_uuid}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["channel"]["code"], "HR");

    // Arsip: bukan hapus; boleh walau masih dipakai customer; idempoten; 404.
    let r = app
        .delete(&format!("{M}/types/{ty_uuid}"))
        .token(admin)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json, json!({"data":{"id": ty_uuid,"archived":true}}));
    let active: bool = app
        .scalar(&format!(
            r#"SELECT "isActive" FROM "customerType" WHERE id = {ty_id}"#
        ))
        .await;
    assert!(!active);
    app.delete(&format!("{M}/types/{ty_uuid}"))
        .token(admin)
        .send()
        .await
        .assert_status(200);
    let r = app
        .delete(&format!("{M}/types/{}", v4()))
        .token(admin)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(
        r.json,
        json!({"code":"CUSTOMER_MASTER_NOT_FOUND","message":"Customer type not found"})
    );
    app.delete(&format!("{M}/types/{ty_uuid}"))
        .token(&c.fx.staff.token)
        .send()
        .await
        .assert_status(401);
    // Dipulihkan lewat PATCH isActive.
    app.patch(&format!("{M}/types/{ty_uuid}"))
        .token(admin)
        .json(&json!({"isActive": true}))
        .send()
        .await
        .assert_status(200);

    // Audit: model customerType ber-audit (tulisan yang hanya mengubah `updatedAt` tidak dicatat); arsip = `delete`, pulih = `restore` (tanda tangan `isActive`).
    let acts = audit_actions(&c, "customerType", ty_id, 4).await;
    assert_eq!(acts, vec!["create", "update", "delete", "restore"]);
    let acts = audit_actions(&c, "customerSubChannel", sub_id, 2).await;
    assert_eq!(acts[0], "create");
}

// ───────────────────────── customers: CRUD ─────────────────────────

struct Masters {
    ty: String,
    ty2: String,
    grp: String,
    ch: String,
    ch2: String,
    sub: String,
    pl: String,
}

async fn seed_masters(c: &Ctx) -> Masters {
    let uuid_of = |v: &Value| v["id"].as_str().unwrap().to_owned();
    let ty = uuid_of(
        &new_master(
            c,
            "types",
            json!({"code":"DISTRIBUTOR","name":"Distributor"}),
        )
        .await,
    );
    let ty2 = uuid_of(&new_master(c, "types", json!({"code":"RETAILER","name":"Retailer"})).await);
    let grp = uuid_of(&new_master(c, "groups", json!({"code":"VIP","name":"VIP"})).await);
    let ch = uuid_of(&new_master(c, "channels", json!({"code":"GT","name":"General Trade"})).await);
    let ch2 = uuid_of(&new_master(c, "channels", json!({"code":"MT","name":"Modern Trade"})).await);
    let sub = uuid_of(
        &new_master(
            c,
            "sub-channels",
            json!({"code":"WRG","name":"Warung","channelId":ch}),
        )
        .await,
    );
    let pl: String = c
        .app
        .scalar(&format!(
            r#"INSERT INTO "priceList" ("companyId", code, name, "currencyCode", "isActive", "createdById")
               VALUES ({}, 'PL1', 'Harga Distributor', 'IDR', false, {}) RETURNING uuid::text"#,
            c.fx.id, c.fx.admin.id
        ))
        .await;
    Masters {
        ty,
        ty2,
        grp,
        ch,
        ch2,
        sub,
        pl,
    }
}

#[tokio::test]
async fn customers_crud_validation_transactions_delete_and_audit() {
    let Some(c) = setup().await else { return };
    let app = &c.app;
    let admin = &c.fx.admin.token;

    // Gerbang.
    app.get(C).send().await.assert_status(401);
    app.post(C)
        .token(&c.fx.staff.token)
        .json(&json!({"code":"A","name":"n"}))
        .send()
        .await
        .assert_status(401);
    app.get(C)
        .token(&c.fx.manager.token)
        .send()
        .await
        .assert_status(200);

    // Tanpa tipe default (belum ada DISTRIBUTOR) → type null, POST tetap jalan (§14.3).
    let r = app
        .post(C)
        .token(admin)
        .json(&json!({"code":"early","name":"Tanpa Tipe"}))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    assert_eq!(r.data()["type"], Value::Null);
    assert_eq!(r.data()["code"], "EARLY");
    let m = seed_masters(&c).await;

    // Validasi create (urutan dekorator).
    let r = app.post(C).token(admin).json(&json!({})).send().await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec![
            CODE_MSG,
            "code must be a string",
            "name must be longer than or equal to 1 characters",
            "name must be a string"
        ]
    );
    let r = app
        .post(C)
        .token(admin)
        .json(&json!({"code":"x y","name":"","typeId":"nope","isActive":"yes","phone":"1".repeat(21)}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec![
            CODE_MSG,
            "name must be longer than or equal to 1 characters",
            "typeId must be a UUID",
            "phone must be shorter than or equal to 20 characters",
            "isActive must be a boolean value"
        ]
    );

    // Create minimal: tipe default DISTRIBUTOR; bentuk & urutan kunci.
    let r = app
        .post(C)
        .token(admin)
        .json(&json!({"code":"cust-001","name":"PT Sumber Makmur"}))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
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
    let a = r.data().clone();
    assert_eq!(a["code"], "CUST-001");
    assert_eq!(a["type"]["code"], "DISTRIBUTOR");
    assert_eq!(a["type"]["id"], m.ty);
    for k in [
        "group",
        "channel",
        "subChannel",
        "priceList",
        "taxNumber",
        "phone",
    ] {
        assert_eq!(a[k], Value::Null, "{k}");
    }
    assert_eq!(a["isActive"], true);
    assert_eq!(a["storeCount"], 0);
    assert!(a["createdAt"].is_i64() && a["updatedAt"].is_i64());
    let a_uuid = a["id"].as_str().unwrap().to_owned();
    let a_id = internal_id(&c, "customer", &a_uuid).await;

    // Create penuh: semua ref, taxNumber & phone dinormalisasi, price list tak aktif tetap boleh.
    let b = new_customer(
        &c,
        json!({
            "code":"CUST-002","name":"Toko Bahagia","typeId":m.ty2,"groupId":m.grp,"channelId":m.ch,
            "subChannelId":m.sub,"priceListId":m.pl,"taxNumber":" NPWP: 01.234.567.8-901.000 ",
            "phone":"0812-3456 7890","isActive":false
        }),
    )
    .await;
    assert_eq!(b["type"]["code"], "RETAILER");
    assert_eq!(b["group"]["code"], "VIP");
    assert_eq!(b["channel"]["code"], "GT");
    assert_eq!(
        b["subChannel"],
        json!({"id":m.sub,"code":"WRG","name":"Warung"})
    );
    assert_eq!(
        b["priceList"],
        json!({"id":m.pl,"code":"PL1","name":"Harga Distributor"})
    );
    assert_eq!(b["taxNumber"], "01.234.567.8-901.000");
    assert_eq!(b["phone"], "+6281234567890");
    assert_eq!(b["isActive"], false);
    let b_uuid = b["id"].as_str().unwrap().to_owned();
    // taxNumber tanpa sisa → null.
    let r = new_customer(
        &c,
        json!({"code":"CUST-003","name":"Agen Jaya","taxNumber":"n/a"}),
    )
    .await;
    assert_eq!(r["taxNumber"], Value::Null);

    // Kode kembar (huruf kecil dinormalkan) → 409; transaksi tidak menyisakan baris.
    let r = app
        .post(C)
        .token(admin)
        .json(&json!({"code":"cust-001","name":"Kembar"}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(
        r.json,
        json!({"code":"CUSTOMER_CODE_TAKEN","message":"Customer code CUST-001 is already used in this company"})
    );
    // Company lain boleh memakai kode yang sama.
    let other = app.other_company_admin().await;
    app.post(C)
        .token(&other.admin.token)
        .json(&json!({"code":"CUST-001","name":"Beda company"}))
        .send()
        .await
        .assert_status(201);

    // Referensi master: asing 404, arsip 422, konsistensi sub channel 422, price list 404.
    let r = app
        .post(C)
        .token(admin)
        .json(&json!({"code":"X1","name":"x","typeId":v4()}))
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(
        r.json,
        json!({"code":"CUSTOMER_MASTER_NOT_FOUND","message":"Customer master row not found"})
    );
    let r = app
        .post(C)
        .token(admin)
        .json(&json!({"code":"X1","name":"x","priceListId":v4()}))
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(
        r.json,
        json!({"code":"PRICE_LIST_NOT_FOUND","message":"Price list not found"})
    );
    let r = app
        .post(C)
        .token(admin)
        .json(&json!({"code":"X1","name":"x","channelId":m.ch2,"subChannelId":m.sub}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(
        r.json,
        json!({"code":"SUBCHANNEL_CHANNEL_MISMATCH","message":"subChannelId does not belong to channelId"})
    );
    let r = app
        .post(C)
        .token(admin)
        .json(&json!({"code":"X1","name":"x","subChannelId":m.sub}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(
        r.json,
        json!({"code":"SUBCHANNEL_WITHOUT_CHANNEL","message":"channelId is required when subChannelId is set"})
    );
    app.delete(&format!("{M}/groups/{}", m.grp))
        .token(admin)
        .send()
        .await
        .assert_status(200);
    let r = app
        .post(C)
        .token(admin)
        .json(&json!({"code":"X1","name":"x","groupId":m.grp}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(
        r.json,
        json!({"code":"CUSTOMER_MASTER_ARCHIVED","message":"Cannot assign an archived customer master row"})
    );
    // Master milik company lain tidak bisa dipasang (404, bukan bocor).
    let foreign: String = app
        .scalar(&format!(
            r#"INSERT INTO "customerType" ("companyId", code, name, "createdById")
               VALUES ({}, 'FOREIGN', 'Foreign', {}) RETURNING uuid::text"#,
            other.id, other.admin.id
        ))
        .await;
    app.post(C)
        .token(admin)
        .json(&json!({"code":"X1","name":"x","typeId":foreign}))
        .send()
        .await
        .assert_status(404);
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM customer WHERE "companyId" = {} AND code = 'X1'"#,
            c.fx.id
        ))
        .await;
    assert_eq!(n, 0, "tidak ada baris sisa dari POST yang ditolak");

    // Detail: 200 + storeCount; 404 / uuid rusak (P2023 → 500) / lintas company.
    app.exec(&format!(
        r#"INSERT INTO store ("companyId", code, name, "ownerName", address, lat, lng, "createdById", "customerId")
           VALUES ({cid}, 'ST-1', 'Toko 1', 'Pemilik', 'Jl. A', 0, 0, {uid}, {a_id}),
                  ({cid}, 'ST-2', 'Toko 2', 'Pemilik', 'Jl. B', 0, 0, {uid}, {a_id})"#,
        cid = c.fx.id,
        uid = c.fx.admin.id
    ))
    .await;
    app.exec(&format!(
        r#"UPDATE store SET "deletedAt" = NOW() WHERE code = 'ST-2' AND "companyId" = {}"#,
        c.fx.id
    ))
    .await;
    let r = app.get(&format!("{C}/{a_uuid}")).token(admin).send().await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(
        r.data()["storeCount"],
        1,
        "toko yang dihapus tidak dihitung"
    );
    let r = app.get(&format!("{C}/{}", v4())).token(admin).send().await;
    r.assert_status(404);
    assert_eq!(
        r.json,
        json!({"code":"CUSTOMER_NOT_FOUND","message":"Customer not found"})
    );
    app.get(&format!("{C}/not-a-uuid"))
        .token(admin)
        .send()
        .await
        .assert_status(500);
    app.get(&format!("{C}/{a_uuid}"))
        .token(&other.admin.token)
        .send()
        .await
        .assert_status(404);

    // Update: field terkirim saja; `null` melepas ref; taxNumber/phone dinormalisasi; updatedAt maju.
    let before_updated = b["updatedAt"].as_i64().unwrap();
    tokio::time::sleep(Duration::from_millis(15)).await;
    let r = app
        .patch(&format!("{C}/{b_uuid}"))
        .token(admin)
        .json(
            &json!({"name":"Toko Bahagia 2","typeId":null,"priceListId":null,"groupId":null,
            "taxNumber":"NPWP 09.876","phone":"08111"}),
        )
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let d = r.data();
    assert_eq!(d["name"], "Toko Bahagia 2");
    assert_eq!(d["type"], Value::Null);
    assert_eq!(d["priceList"], Value::Null);
    assert_eq!(d["group"], Value::Null);
    assert_eq!(d["taxNumber"], "09.876");
    assert_eq!(d["phone"], "+628111");
    assert_eq!(d["channel"]["code"], "GT", "tidak dikirim = tidak disentuh");
    assert_eq!(d["code"], "CUST-002");
    assert!(d["updatedAt"].as_i64().unwrap() > before_updated);
    // PATCH kosong: tak ada yang berubah (kecuali updatedAt).
    app.patch(&format!("{C}/{b_uuid}"))
        .token(admin)
        .json(&json!({}))
        .send()
        .await
        .assert_status(200);

    // 3b: konsistensi atas keadaan AKHIR.
    let r = app
        .patch(&format!("{C}/{b_uuid}"))
        .token(admin)
        .json(&json!({"channelId": m.ch2}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(r.json["code"], "SUBCHANNEL_CHANNEL_MISMATCH");
    let r = app
        .patch(&format!("{C}/{b_uuid}"))
        .token(admin)
        .json(&json!({"channelId": null}))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(r.json["code"], "SUBCHANNEL_WITHOUT_CHANNEL");
    let r = app
        .patch(&format!("{C}/{b_uuid}"))
        .token(admin)
        .json(&json!({"channelId": m.ch2, "subChannelId": null}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["channel"]["code"], "MT");
    assert_eq!(r.data()["subChannel"], Value::Null);
    // Ref arsip/asing/price list asing pada PATCH.
    app.patch(&format!("{C}/{b_uuid}"))
        .token(admin)
        .json(&json!({"groupId": m.grp}))
        .send()
        .await
        .assert_status(422);
    app.patch(&format!("{C}/{b_uuid}"))
        .token(admin)
        .json(&json!({"priceListId": v4()}))
        .send()
        .await
        .assert_status(404);
    // Kode baru bentrok → 409; PATCH validasi; null pada kolom non-null → 500.
    let r = app
        .patch(&format!("{C}/{b_uuid}"))
        .token(admin)
        .json(&json!({"code": "cust-001"}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "CUSTOMER_CODE_TAKEN");
    assert_eq!(
        r.json["message"],
        "Customer code CUST-001 is already used in this company"
    );
    let r = app
        .patch(&format!("{C}/{b_uuid}"))
        .token(admin)
        .json(&json!({"code":"bad code","subChannelId":"x"}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(messages(&r), vec![CODE_MSG, "subChannelId must be a UUID"]);
    for body in [
        json!({"name":null}),
        json!({"isActive":null}),
        json!({"code":null}),
    ] {
        app.patch(&format!("{C}/{b_uuid}"))
            .token(admin)
            .json(&body)
            .send()
            .await
            .assert_status(500);
    }
    app.patch(&format!("{C}/{}", v4()))
        .token(admin)
        .json(&json!({"name":"x"}))
        .send()
        .await
        .assert_status(404);
    app.patch(&format!("{C}/{a_uuid}"))
        .token(&other.admin.token)
        .json(&json!({"name":"hack"}))
        .send()
        .await
        .assert_status(404);

    // Delete: 409 selama masih punya toko hidup; lalu soft delete; kode boleh dipakai ulang.
    let r = app
        .delete(&format!("{C}/{a_uuid}"))
        .token(admin)
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(
        r.json,
        json!({"statusCode":409,"error":"CUSTOMER_HAS_STORES","message":"Customer still has 1 stores. Reassign or remove them first."})
    );
    app.delete(&format!("{C}/{a_uuid}"))
        .token(&c.fx.staff.token)
        .send()
        .await
        .assert_status(401);
    app.exec(&format!(
        r#"UPDATE store SET "deletedAt" = NOW() WHERE "customerId" = {a_id}"#
    ))
    .await;
    let r = app
        .delete(&format!("{C}/{a_uuid}"))
        .token(admin)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json, json!({"data":{"id":a_uuid,"deleted":true}}));
    let deleted: bool = app
        .scalar(&format!(
            r#"SELECT "deletedAt" IS NOT NULL FROM customer WHERE id = {a_id}"#
        ))
        .await;
    assert!(deleted, "soft delete, bukan hapus baris");
    app.get(&format!("{C}/{a_uuid}"))
        .token(admin)
        .send()
        .await
        .assert_status(404);
    app.delete(&format!("{C}/{a_uuid}"))
        .token(admin)
        .send()
        .await
        .assert_status(404);
    app.patch(&format!("{C}/{a_uuid}"))
        .token(admin)
        .json(&json!({"name":"x"}))
        .send()
        .await
        .assert_status(404);
    new_customer(&c, json!({"code":"CUST-001","name":"Kode dipakai ulang"})).await;

    // Audit: create → delete (soft delete lewat tanda tangan `deletedAt`).
    let acts = audit_actions(&c, "customer", a_id, 2).await;
    assert_eq!(acts, vec!["create", "delete"]);
    let b_id = internal_id(&c, "customer", &b_uuid).await;
    let acts = audit_actions(&c, "customer", b_id, 3).await;
    assert_eq!(acts[0], "create");
    assert!(acts[1..].iter().all(|a| a == "update"), "{acts:?}");
}

// ───────────────────────── customers: list & export ─────────────────────────

fn codes(r: &TestResponse) -> Vec<String> {
    r.json["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["code"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn customers_list_filters_sort_paging_and_export() {
    let Some(c) = setup().await else { return };
    let app = &c.app;
    let admin = &c.fx.admin.token;
    let m = seed_masters(&c).await;

    new_customer(
        &c,
        json!({"code":"CUST-001","name":"PT Sumber Makmur","taxNumber":"01.234.567.8-901.000","typeId":m.ty}),
    )
    .await;
    new_customer(
        &c,
        json!({"code":"CUST-002","name":"Toko Bahagia","typeId":m.ty2,"channelId":m.ch}),
    )
    .await;
    new_customer(
        &c,
        json!({"code":"CUST-003","name":"Agen Jaya","isActive":false,"groupId":m.grp}),
    )
    .await;
    let del = new_customer(&c, json!({"code":"CUST-004","name":"Sudah Dihapus"})).await;
    app.delete(&format!("{C}/{}", del["id"].as_str().unwrap()))
        .token(admin)
        .send()
        .await
        .assert_status(200);
    // Toko hidup untuk CUST-001 (hasStores).
    let id1: i32 = app
        .scalar(r#"SELECT id FROM customer WHERE code = 'CUST-001' ORDER BY id DESC LIMIT 1"#)
        .await;
    app.exec(&format!(
        r#"INSERT INTO store ("companyId", code, name, "ownerName", address, lat, lng, "createdById", "customerId")
           VALUES ({}, 'ST-L1', 'Toko L1', 'P', 'Jl', 0, 0, {}, {id1})"#,
        c.fx.id, c.fx.admin.id
    ))
    .await;

    // Default: name ASC (kolasi DB), baris terhapus tidak ikut, meta.
    let r = app.get(C).token(admin).send().await;
    r.assert_status(200).assert_no_sql_error();
    let got = codes(&r);
    assert_eq!(got.len(), 3);
    assert_eq!(
        r.json["meta"],
        json!({"total":3,"page":1,"limit":20,"totalPages":1})
    );
    assert!(
        r.json.get("fieldPolicy").is_none(),
        "tanpa kebijakan tak ada fieldPolicy"
    );
    let names: Vec<String> = r.json["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap().to_lowercase())
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "urut name ASC");
    let with_store = r.json["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["code"] == "CUST-001")
        .unwrap();
    assert_eq!(with_store["storeCount"], 1);

    // Sort + arah; tie-break id.
    let r = app
        .get(&format!("{C}?sortBy=code&sortDir=desc"))
        .token(admin)
        .send()
        .await;
    assert_eq!(codes(&r), vec!["CUST-003", "CUST-002", "CUST-001"]);
    let r = app
        .get(&format!("{C}?sortBy=createdAt"))
        .token(admin)
        .send()
        .await;
    assert_eq!(codes(&r), vec!["CUST-001", "CUST-002", "CUST-003"]);
    let r = app
        .get(&format!("{C}?sortBy=code"))
        .token(admin)
        .send()
        .await;
    assert_eq!(codes(&r), vec!["CUST-001", "CUST-002", "CUST-003"]);

    // Search: code / name (case-insensitive) / taxNumber.
    for (q, want) in [
        ("sumber", vec!["CUST-001"]),
        ("AGEN", vec!["CUST-003"]),
        ("234.567", vec!["CUST-001"]),
        ("zzz", vec![]),
    ] {
        let r = app
            .get(&format!("{C}?search={q}"))
            .token(admin)
            .send()
            .await;
        r.assert_status(200);
        assert_eq!(codes(&r), want, "search={q}");
    }
    let r = app
        .get(&format!("{C}?search=cust-00&sortBy=code"))
        .token(admin)
        .send()
        .await;
    assert_eq!(codes(&r), vec!["CUST-001", "CUST-002", "CUST-003"]);

    // Filter uuid[]: satu, daftar koma, param berulang, `[]`, uuid asing (= tak ada baris).
    let r = app
        .get(&format!("{C}?typeId={}", m.ty2))
        .token(admin)
        .send()
        .await;
    assert_eq!(codes(&r), vec!["CUST-002"]);
    // CUST-003 dibuat tanpa typeId → tipe default DISTRIBUTOR (§14.3).
    let r = app
        .get(&format!("{C}?typeId={}&sortBy=code", m.ty))
        .token(admin)
        .send()
        .await;
    assert_eq!(codes(&r), vec!["CUST-001", "CUST-003"]);
    let r = app
        .get(&format!("{C}?typeId={},{}&sortBy=code", m.ty, m.ty2))
        .token(admin)
        .send()
        .await;
    assert_eq!(codes(&r), vec!["CUST-001", "CUST-002", "CUST-003"]);
    let r = app
        .get(&format!("{C}?typeId={}&typeId={}", m.ty, m.ty2))
        .token(admin)
        .send()
        .await;
    assert_eq!(r.json["meta"]["total"], 3);
    let r = app
        .get(&format!("{C}?groupId[]={}", m.grp))
        .token(admin)
        .send()
        .await;
    assert_eq!(codes(&r), vec!["CUST-003"]);
    let r = app
        .get(&format!("{C}?channelId={}", m.ch))
        .token(admin)
        .send()
        .await;
    assert_eq!(codes(&r), vec!["CUST-002"]);
    let r = app
        .get(&format!("{C}?typeId={}", v4()))
        .token(admin)
        .send()
        .await;
    assert_eq!(r.json["meta"]["total"], 0);
    // typeId kosong = tanpa filter.
    let r = app.get(&format!("{C}?typeId=")).token(admin).send().await;
    assert_eq!(r.json["meta"]["total"], 3);

    // isActive & hasStores (kedua nilai bermakna).
    let r = app
        .get(&format!("{C}?isActive=false"))
        .token(admin)
        .send()
        .await;
    assert_eq!(codes(&r), vec!["CUST-003"]);
    let r = app
        .get(&format!("{C}?isActive=true"))
        .token(admin)
        .send()
        .await;
    assert_eq!(r.json["meta"]["total"], 2);
    let r = app
        .get(&format!("{C}?hasStores=true"))
        .token(admin)
        .send()
        .await;
    assert_eq!(codes(&r), vec!["CUST-001"]);
    let r = app
        .get(&format!("{C}?hasStores=false&sortBy=code"))
        .token(admin)
        .send()
        .await;
    assert_eq!(codes(&r), vec!["CUST-002", "CUST-003"]);

    // Paging.
    let r = app
        .get(&format!("{C}?page=2&limit=2&sortBy=code"))
        .token(admin)
        .send()
        .await;
    assert_eq!(codes(&r), vec!["CUST-003"]);
    assert_eq!(
        r.json["meta"],
        json!({"total":3,"page":2,"limit":2,"totalPages":2})
    );
    let r = app.get(&format!("{C}?page=9")).token(admin).send().await;
    assert_eq!(r.json["data"], json!([]));

    // Validasi query.
    let r = app
        .get(&format!(
            "{C}?typeId=x&isActive=maybe&hasStores=1&sortBy=id&sortDir=up&page=0&limit=101"
        ))
        .token(admin)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec![
            "each value in typeId must be a UUID",
            "isActive must be a boolean value",
            "hasStores must be a boolean value",
            "sortBy must be one of the following values: name, code, createdAt",
            "sortDir must be one of the following values: asc, desc",
            "page must not be less than 1",
            "limit must not be greater than 100"
        ]
    );
    let r = app
        .get(&format!("{C}?search={}", "a".repeat(121)))
        .token(admin)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        messages(&r),
        vec!["search must be shorter than or equal to 120 characters"]
    );
    // Isolasi company.
    let other = app.other_company_admin().await;
    let r = app.get(C).token(&other.admin.token).send().await;
    assert!(!codes(&r).iter().any(|c| c.starts_with("CUST-00")));

    // ── export ──
    app.get(&format!("{C}/export"))
        .send()
        .await
        .assert_status(401);
    app.get(&format!("{C}/export"))
        .token(&c.fx.staff.token)
        .send()
        .await
        .assert_status(401);
    let r = app.get(&format!("{C}/export")).token(admin).send().await;
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
    let text = shared_strings(&r.bytes);
    for needle in [
        "CUST-001",
        "CUST-002",
        "CUST-003",
        "01.234.567.8-901.000",
        ">storeCount<",
        "DISTRIBUTOR \u{2014} Distributor",
    ] {
        assert!(text.contains(needle), "{needle} tidak ada di export");
    }
    assert!(!text.contains("CUST-004"), "baris terhapus tidak diekspor");
    // Filter sama dengan list; tanpa page/limit.
    let r = app
        .get(&format!("{C}/export?search=sumber&page=zzz&limit=0"))
        .token(admin)
        .send()
        .await;
    r.assert_status(200);
    let text = shared_strings(&r.bytes);
    assert!(text.contains("CUST-001") && !text.contains("CUST-002"));
    app.get(&format!("{C}/export?isActive=maybe"))
        .token(admin)
        .send()
        .await
        .assert_status(400);
    // `export` tidak ditelan `{id}`; method lain ke path statis: 405 di router ini, 404 `Cannot POST` lewat `mz-server`.
    app.post(&format!("{C}/export"))
        .token(admin)
        .json(&json!({}))
        .send()
        .await
        .assert_status(405);
}

// ───────────────────────── kebijakan kolom + reveal ─────────────────────────

async fn set_policy(c: &Ctx, rows: &str) {
    let cid = c.fx.id;
    let insert = if rows.is_empty() {
        String::new()
    } else {
        format!(
            r#"INSERT INTO "tableFieldVisibility" ("companyId", "accessLevelId", "tableKey", "columnKey", mode)
               VALUES {};"#,
            rows.replace("CID", &cid.to_string())
        )
    };
    c.app
        .exec(&format!(
            r#"DELETE FROM "tableFieldVisibility" WHERE "companyId" = {cid}; {insert}"#
        ))
        .await;
    mz_table_fields::policy::invalidate_company_local(cid);
}

#[tokio::test]
async fn customers_field_policy_search_sort_export_and_reveal() {
    let Some(c) = setup().await else { return };
    let app = &c.app;
    let admin = &c.fx.admin.token;
    let m = seed_masters(&c).await;
    let a = new_customer(
        &c,
        json!({"code":"CUST-001","name":"PT Sumber Makmur","taxNumber":"01.234.567.8-901.000",
               "phone":"08123456789","typeId":m.ty}),
    )
    .await;
    let a_uuid = a["id"].as_str().unwrap().to_owned();

    // taxNumber hidden + phone masked.
    set_policy(
        &c,
        "(CID, NULL, 'customers', 'taxNumber', 'hidden'), (CID, NULL, 'customers', 'phone', 'masked')",
    )
    .await;
    let r = app.get(C).token(admin).send().await;
    r.assert_status(200);
    assert_eq!(r.json["data"][0]["taxNumber"], Value::Null);
    assert_eq!(r.json["data"][0]["phone"], Value::Null);
    assert_eq!(r.json["data"][0]["code"], "CUST-001");
    assert_eq!(
        r.json["fieldPolicy"],
        json!({"tableKey":"customers","hidden":["taxNumber"],"masked":["phone"],"revealable":[]})
    );
    assert_key_order(&r.text(), &["data", "meta", "fieldPolicy"]);
    let r = app.get(&format!("{C}/{a_uuid}")).token(admin).send().await;
    assert_eq!(r.data()["taxNumber"], Value::Null);
    assert_eq!(r.json["fieldPolicy"]["tableKey"], "customers");
    // Echo create & update ikut disensor (nilai di DB tetap utuh).
    let r = app
        .post(C)
        .token(admin)
        .json(&json!({"code":"CUST-002","name":"Toko Bahagia","taxNumber":"99.999"}))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(r.data()["taxNumber"], Value::Null);
    assert!(r.json.get("fieldPolicy").is_some());
    let stored: String = app
        .scalar(r#"SELECT "taxNumber" FROM customer WHERE code = 'CUST-002' AND "deletedAt" IS NULL LIMIT 1"#)
        .await;
    assert_eq!(stored, "99.999");
    let r = app
        .patch(&format!("{C}/{a_uuid}"))
        .token(admin)
        .json(&json!({"name":"PT Sumber Makmur Baru"}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.data()["taxNumber"], Value::Null);
    // DELETE bukan baris customer: tanpa fieldPolicy.
    let tmp = new_customer(&c, json!({"code":"TMP","name":"Tmp"})).await;
    let r = app
        .delete(&format!("{C}/{}", tmp["id"].as_str().unwrap()))
        .token(admin)
        .send()
        .await;
    assert!(r.json.get("fieldPolicy").is_none());

    // §6.4: search tidak lagi mencocokkan kolom tersensor, code/name tetap.
    let r = app
        .get(&format!("{C}?search=234.567"))
        .token(admin)
        .send()
        .await;
    assert_eq!(
        r.json["meta"]["total"], 0,
        "taxNumber tersensor tak boleh bocor lewat search"
    );
    let r = app
        .get(&format!("{C}?search=cust-001"))
        .token(admin)
        .send()
        .await;
    assert_eq!(r.json["meta"]["total"], 1);
    // §6.3: export membuang kolom hidden, mengosongkan masked.
    let r = app.get(&format!("{C}/export")).token(admin).send().await;
    r.assert_status(200);
    let text = shared_strings(&r.bytes);
    assert!(!text.contains("01.234.567.8-901.000") && !text.contains("99.999"));
    assert!(!text.contains(">taxNumber<"), "kolom hidden dibuang");
    assert!(
        text.contains(">phone<") && !text.contains("+628123456789"),
        "masked: judul ada, nilai kosong"
    );

    // sort ke kolom tersensor → 400 FIELD_NOT_SORTABLE; kolom lain tetap boleh.
    set_policy(&c, "(CID, NULL, 'customers', 'createdAt', 'hidden')").await;
    let r = app
        .get(&format!("{C}?sortBy=createdAt"))
        .token(admin)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["code"], "FIELD_NOT_SORTABLE");
    assert_eq!(
        r.json["details"],
        json!({"tableKey":"customers","field":"createdAt","mode":"hidden","label":"Dibuat"})
    );
    app.get(&format!("{C}?sortBy=code"))
        .token(admin)
        .send()
        .await
        .assert_status(200);

    // ── reveal (POST /v1/admin/table-fields/reveal) lewat resolver `customers` ──
    set_policy(
        &c,
        "(CID, NULL, 'customers', 'taxNumber', 'revealable'), (CID, NULL, 'customers', 'phone', 'masked')",
    )
    .await;
    let reveal = "/v1/admin/table-fields/reveal";
    let r = app.get(&format!("{C}/{a_uuid}")).token(admin).send().await;
    assert_eq!(r.json["fieldPolicy"]["revealable"], json!(["taxNumber"]));
    let r = app
        .post(reveal)
        .token(admin)
        .json(&json!({"tableKey":"customers","recordId":a_uuid,"field":"taxNumber"}))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(
        r.json,
        json!({"data":{"field":"taxNumber","value":"01.234.567.8-901.000"}})
    );
    let (label, rid): (String, i32) = sqlx::query_as(
        r#"SELECT "recordLabel", "recordId" FROM "fieldRevealLog"
            WHERE "companyId" = $1 AND "tableKey" = 'customers' AND "columnKey" = 'taxNumber'"#,
    )
    .bind(c.fx.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(label, "CUST-001 \u{2014} PT Sumber Makmur Baru");
    assert_eq!(rid, internal_id(&c, "customer", &a_uuid).await);
    // Kolom bukan revealable (masked) → 403; customer tak ada → 404 milik customers.
    let r = app
        .post(reveal)
        .token(admin)
        .json(&json!({"tableKey":"customers","recordId":a_uuid,"field":"phone"}))
        .send()
        .await;
    r.assert_status(403);
    assert_eq!(r.json["code"], "FIELD_NOT_REVEALABLE");
    let r = app
        .post(reveal)
        .token(admin)
        .json(&json!({"tableKey":"customers","recordId":v4(),"field":"taxNumber"}))
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(r.json["code"], "CUSTOMER_NOT_FOUND");
    // Customer company lain tidak bisa di-reveal (404, bukan nilai).
    let other = app.other_company_admin().await;
    app.exec(&format!(
        r#"INSERT INTO "tableFieldVisibility" ("companyId", "accessLevelId", "tableKey", "columnKey", mode)
           VALUES ({}, NULL, 'customers', 'taxNumber', 'revealable')"#,
        other.id
    ))
    .await;
    mz_table_fields::policy::invalidate_company_local(other.id);
    let r = app
        .post(reveal)
        .token(&other.admin.token)
        .json(&json!({"tableKey":"customers","recordId":a_uuid,"field":"taxNumber"}))
        .send()
        .await;
    r.assert_status(404);
    app.exec(&format!(
        r#"DELETE FROM "tableFieldVisibility" WHERE "companyId" = {}"#,
        other.id
    ))
    .await;
    mz_table_fields::policy::invalidate_company_local(other.id);
}
