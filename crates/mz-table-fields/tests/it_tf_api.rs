//! Uji integrasi 6 route `/v1/admin/table-fields/*` lewat HTTP (gerbang lengkap + layer audit)
//! terhadap Postgres + Redis nyata, termasuk pesan invalidasi `table-field-policy`.
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-table-fields --test it_tf_api`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use fred::interfaces::{ClientLike, EventInterface, PubsubInterface};
use fred::prelude::Builder;
use futures::FutureExt;
use futures::future::BoxFuture;
use http::StatusCode;
use mz_core::{ApiError, AppState};
use mz_table_fields::error::ReqError;
use mz_table_fields::registry::{
    CUSTOMERS_REVEAL_RESOLVER, FieldVisibilityMode, STORES_REVEAL_RESOLVER,
};
use mz_table_fields::reveal::{RevealRequest, RevealResolver, RevealTarget, register_resolver};
use mz_testkit::{Company, TestApp};
use serde_json::{Value, json};
use tokio::sync::Mutex;

/// Semua test memakai company fixture yang sama: dijalankan berurutan.
static SERIAL: Mutex<()> = Mutex::const_new(());

const BASE: &str = "/v1/admin/table-fields";
const STORE_UUID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

fn build(state: &AppState) -> Router {
    mz_table_fields::router(state)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

/// Resolver uji: satu baris yang dikenal, selebihnya 404 milik tabel itu sendiri.
struct FakeResolver(&'static str, &'static str);

impl RevealResolver for FakeResolver {
    fn resolve<'a>(
        &'a self,
        _state: &'a AppState,
        request: RevealRequest,
    ) -> BoxFuture<'a, Result<RevealTarget, ReqError>> {
        let (code, label) = (self.0, self.1);
        async move {
            if request.record_uuid != STORE_UUID {
                return Err(ApiError::coded(StatusCode::NOT_FOUND, code, "not found"));
            }
            Ok(RevealTarget {
                record_id: 4242,
                record_uuid: Some(STORE_UUID.to_owned()),
                record_label: Some(label.to_owned()),
                record: json!({ "phone": "0812-000", "ownerName": "Bu Ani", "taxNumber": "01.234" }),
            })
        }
        .boxed()
    }
}

struct Ctx {
    app: TestApp,
    c: Company,
    channel: String,
    staff_role: String,
}

async fn setup() -> Option<Ctx> {
    let channel = format!("mz:test:tf:{}", uuid::Uuid::new_v4().simple());
    let app = TestApp::with_vars(build, &[("CACHE_INVALIDATION_CHANNEL", &channel)]).await?;
    let c = app.company().await;
    app.exec(&format!(
        r#"DELETE FROM "tableFieldVisibility" WHERE "companyId" = {0};
           DELETE FROM "fieldRevealLog" WHERE "companyId" = {0};
           DELETE FROM "fieldRevealQuota" WHERE "companyId" = {0};
           INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
           SELECT al.id, f.k, 'READ'::"PermissionAction" FROM "accessLevel" al
           CROSS JOIN (VALUES ('table-fields'), ('stores')) AS f(k)
           WHERE al."isSystem" AND al.code IN ('SYSTEM_ADMIN', 'SUPER_ADMIN', 'ADMIN', 'MANAGER')
           ON CONFLICT DO NOTHING;
           INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
           SELECT al.id, 'customers', 'READ'::"PermissionAction" FROM "accessLevel" al
           WHERE al."isSystem" AND al.code IN ('SYSTEM_ADMIN', 'SUPER_ADMIN', 'ADMIN')
           ON CONFLICT DO NOTHING;
           DELETE FROM "rolePermission" rp USING "accessLevel" al
            WHERE rp."accessLevelId" = al.id AND al."isSystem" AND al.code = 'MANAGER'
              AND rp."featureKey" = 'customers';"#,
        c.id
    ))
    .await;
    register_resolver(
        STORES_REVEAL_RESOLVER,
        Arc::new(FakeResolver("STORE_NOT_FOUND", "TK-001 Toko Uji")),
    );
    register_resolver(
        CUSTOMERS_REVEAL_RESOLVER,
        Arc::new(FakeResolver("CUSTOMER_NOT_FOUND", "CU-001 Customer Uji")),
    );
    let staff_role: String = app
        .scalar(
            r#"SELECT uuid::text FROM "accessLevel" WHERE "isSystem" AND code = 'STAFF' LIMIT 1"#,
        )
        .await;
    Some(Ctx {
        app,
        c,
        channel,
        staff_role,
    })
}

/// Subscriber mentah untuk memeriksa pesan protokol invalidasi.
async fn subscribe(
    channel: &str,
) -> (
    fred::clients::SubscriberClient,
    tokio::sync::broadcast::Receiver<fred::types::Message>,
) {
    let client = Builder::default_centralized()
        .build_subscriber_client()
        .unwrap();
    client.init().await.unwrap();
    let rx = client.message_rx();
    client.subscribe(channel).await.unwrap();
    (client, rx)
}

async fn next_message(
    rx: &mut tokio::sync::broadcast::Receiver<fred::types::Message>,
) -> Option<Value> {
    let m = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .ok()?
        .ok()?;
    serde_json::from_str(&m.value.as_str()?).ok()
}

fn put_body(rows: Value) -> Value {
    json!({ "tableKey": "stores", "rows": rows })
}

#[tokio::test]
async fn it_tf_config_matrix_and_invalidation() {
    let _g = SERIAL.lock().await;
    let Some(ctx) = setup().await else { return };
    let (app, c) = (&ctx.app, &ctx.c);
    let admin = &c.admin.token;
    let (_sub, mut rx) = subscribe(&ctx.channel).await;

    // ── gerbang: tanpa token 401; MANAGER/STAFF 401 (`@Roles` ADMIN ke atas).
    for path in ["tables", "matrix?tableKey=stores", "reveal-logs"] {
        app.get(&format!("{BASE}/{path}"))
            .send()
            .await
            .assert_status(401);
        app.get(&format!("{BASE}/{path}"))
            .token(&c.manager.token)
            .send()
            .await
            .assert_status(401);
    }

    // ── tables.
    let res = app.get(&format!("{BASE}/tables")).token(admin).send().await;
    res.assert_status(200).assert_no_sql_error();
    let data = res.json["data"].as_array().unwrap();
    assert_eq!(data.len(), 5);
    assert_eq!(data[0]["key"], "stores");
    // Hanya kolom yang tidak `locked`; properti `undefined` tidak dikirim.
    assert_eq!(data[0]["columns"].as_array().unwrap().len(), 11);
    assert!(res.text().starts_with(
        r#"{"data":[{"key":"stores","label":"Toko","labelEn":"Stores","columns":[{"key":"ownerName","label":"Nama Pemilik","labelEn":"Owner Name","searchable":true},{"key":"phone","label":"No. Telepon","labelEn":"Phone"},"#
    ), "{}", res.text());

    // ── matrix GET: validasi + bentuk.
    let res = app
        .get(&format!("{BASE}/matrix?tableKey=nope"))
        .token(admin)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "tableKey must be one of the following values: stores, customers, products, orders, order_items"
        ])
    );
    let res = app
        .get(&format!("{BASE}/matrix?tableKey=stores"))
        .token(admin)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    let m = &res.json["data"];
    assert_eq!(m["tableKey"], "stores");
    assert_eq!(m["companyDefault"], json!({}));
    let roles = m["roles"].as_array().unwrap();
    assert!(roles.iter().all(|r| r["code"] != "SYSTEM_ADMIN"));
    assert!(
        roles
            .iter()
            .any(|r| r["code"] == "STAFF" && r["isSystem"] == true)
    );
    assert!(
        res.text()
            .contains(r#""companyDefault":{},"roles":[{"accessLevelId":""#)
    );

    // ── PUT: validasi.
    let res = app
        .put(&format!("{BASE}/matrix"))
        .token(admin)
        .json(&json!({}))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "tableKey must be one of the following values: stores, customers, products, orders, order_items",
            "tableKey must be a string",
            "rows should not be empty",
            "rows must be an array"
        ])
    );
    // Kolom `locked` → 422 FIELD_LOCKED; kolom kembar → 400; role tak dikenal → 404.
    let res = app
        .put(&format!("{BASE}/matrix"))
        .token(admin)
        .json(&put_body(
            json!([{ "entries": [{ "columnKey": "lat", "mode": "hidden" }] }]),
        ))
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(
        res.text(),
        r#"{"code":"FIELD_LOCKED","message":"Column 'lat' on table 'stores' cannot be configured"}"#
    );
    let res = app
        .put(&format!("{BASE}/matrix"))
        .token(admin)
        .json(&put_body(json!([{ "entries": [
            { "columnKey": "phone", "mode": "hidden" }, { "columnKey": "phone", "mode": "masked" }] }])))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        "Column 'phone' repeated in the same row"
    );
    let res = app
        .put(&format!("{BASE}/matrix"))
        .token(admin)
        .json(&put_body(json!([{
            "accessLevelId": "11111111-1111-4111-8111-111111111111",
            "entries": [{ "columnKey": "phone", "mode": "hidden" }] }])))
        .send()
        .await;
    res.assert_status(404);
    assert_eq!(res.json["code"], "ACCESS_LEVEL_NOT_FOUND");
    // Tidak ada yang tertulis oleh permintaan yang ditolak.
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "tableFieldVisibility" WHERE "companyId" = {}"#,
            c.id
        ))
        .await;
    assert_eq!(n, 0);

    // ── PUT sukses: default `visible` tidak disimpan; override role `visible` disimpan.
    let res = app
        .put(&format!("{BASE}/matrix"))
        .token(admin)
        .json(&put_body(json!([
            { "entries": [
                { "columnKey": "phone", "mode": "masked" },
                { "columnKey": "address", "mode": "visible" } ] },
            { "accessLevelId": ctx.staff_role, "entries": [
                { "columnKey": "phone", "mode": "revealable" },
                { "columnKey": "address", "mode": "visible" } ] }
        ])))
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    let m = &res.json["data"];
    assert_eq!(m["companyDefault"], json!({ "phone": "masked" }));
    let staff = m["roles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["accessLevelId"] == ctx.staff_role.as_str())
        .unwrap();
    assert_eq!(
        staff["overrides"],
        json!({ "phone": "revealable", "address": "visible" })
    );
    // Urutan kunci override = urutan penyisipan (phone, address), bukan alfabet.
    assert!(
        res.text()
            .contains(r#""overrides":{"phone":"revealable","address":"visible"}"#)
    );
    // Pesan invalidasi protokol v1 terkirim.
    let msg = next_message(&mut rx)
        .await
        .expect("pesan table-field-policy");
    assert_eq!(msg["v"], 1);
    assert_eq!(msg["kind"], "table-field-policy");
    assert_eq!(msg["companyId"], c.id);
    assert!(msg["src"].as_str().unwrap().starts_with("rust-"));

    // ── Cache kebijakan (pub API untuk route sales-tracker).
    let role_id: i32 = app
        .scalar(&format!(
            r#"SELECT id FROM "accessLevel" WHERE uuid = '{}'"#,
            ctx.staff_role
        ))
        .await;
    let st = &app.state;
    let p = mz_table_fields::policy::resolve(st, c.id, Some(role_id))
        .await
        .unwrap();
    assert_eq!(
        p.mode_for("stores", "phone"),
        FieldVisibilityMode::Revealable
    );
    assert_eq!(
        p.mode_for("stores", "address"),
        FieldVisibilityMode::Visible
    );
    let p_default = mz_table_fields::policy::resolve(st, c.id, None)
        .await
        .unwrap();
    assert_eq!(
        p_default.mode_for("stores", "phone"),
        FieldVisibilityMode::Masked
    );
    // Perubahan langsung di DB tidak terlihat sampai invalidasi (cache 30 dtk)...
    app.exec(&format!(
        r#"UPDATE "tableFieldVisibility" SET mode = 'hidden' WHERE "companyId" = {} AND "accessLevelId" IS NULL"#,
        c.id
    ))
    .await;
    let cached = mz_table_fields::policy::resolve(st, c.id, None)
        .await
        .unwrap();
    assert_eq!(
        cached.mode_for("stores", "phone"),
        FieldVisibilityMode::Masked
    );
    // ...dan `resolve_uncached` selalu membaca DB.
    let fresh = mz_table_fields::policy::resolve_uncached(st, c.id, None)
        .await
        .unwrap();
    assert_eq!(
        fresh.mode_for("stores", "phone"),
        FieldVisibilityMode::Hidden
    );
    // Pesan `table-field-policy` dari Node (diterapkan subscriber core) membuang entri basi.
    mz_core::invalidation::apply(
        &st.caches,
        &mz_core::invalidation::Message::TableFieldPolicy {
            company_id: Some(c.id),
        },
    );
    let again = mz_table_fields::policy::resolve(st, c.id, None)
        .await
        .unwrap();
    assert_eq!(
        again.mode_for("stores", "phone"),
        FieldVisibilityMode::Hidden
    );
    // invalidate_company mem-publish pesan protokol.
    mz_table_fields::policy::invalidate_company(st, c.id);
    let msg = next_message(&mut rx)
        .await
        .expect("pesan invalidate_company");
    assert_eq!(msg["kind"], "table-field-policy");
    mz_table_fields::policy::invalidate_all(st);
    let msg = next_message(&mut rx).await.expect("pesan invalidate_all");
    assert_eq!(msg["kind"], "table-field-policy");
    assert!(msg.get("companyId").is_none());

    // ── PUT REPLACE: baris role lama hilang, kolom lain tidak tersentuh.
    let res = app
        .put(&format!("{BASE}/matrix"))
        .token(admin)
        .json(&put_body(
            json!([{ "accessLevelId": ctx.staff_role, "entries": [
            { "columnKey": "ownerName", "mode": "hidden" }] }]),
        ))
        .send()
        .await;
    res.assert_status(200);
    let staff = res.json["data"]["roles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["accessLevelId"] == ctx.staff_role.as_str())
        .unwrap()
        .clone();
    assert_eq!(staff["overrides"], json!({ "ownerName": "hidden" }));
    assert_eq!(
        res.json["data"]["companyDefault"],
        json!({ "phone": "hidden" })
    );
    let _ = next_message(&mut rx).await;

    // ── DELETE (reset): validasi, reset role, reset default.
    let res = app
        .delete(&format!("{BASE}/matrix?tableKey=stores&accessLevelId=zz"))
        .token(admin)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(res.json["message"], json!(["accessLevelId must be a UUID"]));
    let res = app
        .delete(&format!(
            "{BASE}/matrix?tableKey=stores&accessLevelId={}",
            ctx.staff_role
        ))
        .token(admin)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert!(
        res.json["data"]["roles"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["overrides"] == json!({}))
    );
    assert_eq!(
        res.json["data"]["companyDefault"],
        json!({ "phone": "hidden" })
    );
    let msg = next_message(&mut rx).await.expect("pesan reset");
    assert_eq!(msg["kind"], "table-field-policy");
    let res = app
        .delete(&format!("{BASE}/matrix?tableKey=stores"))
        .token(admin)
        .send()
        .await;
    assert_eq!(res.json["data"]["companyDefault"], json!({}));
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "tableFieldVisibility" WHERE "companyId" = {}"#,
            c.id
        ))
        .await;
    assert_eq!(n, 0);
}

#[tokio::test]
async fn it_tf_reveal_and_logs() {
    let _g = SERIAL.lock().await;
    let Some(ctx) = setup().await else { return };
    let (app, c) = (&ctx.app, &ctx.c);
    let (admin, manager) = (&c.admin.token, &c.manager.token);
    let reveal = format!("{BASE}/reveal");
    let body = |table: &str, field: &str| json!({ "tableKey": table, "recordId": STORE_UUID, "field": field });

    // Gerbang: tanpa token 401, STAFF 401.
    app.post(&reveal)
        .json(&body("stores", "phone"))
        .send()
        .await
        .assert_status(401);
    app.post(&reveal)
        .token(&c.staff.token)
        .json(&body("stores", "phone"))
        .send()
        .await
        .assert_status(401);
    // Validasi.
    let res = app
        .post(&reveal)
        .token(manager)
        .json(
            &json!({ "tableKey": "stores", "recordId": "x", "field": "p".repeat(49), "reason": 5 }),
        )
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "recordId must be a UUID",
            "field must be shorter than or equal to 48 characters",
            "reason must be shorter than or equal to 255 characters",
            "reason must be a string"
        ])
    );

    // Tidak revealable: kolom `visible` (tanpa konfigurasi), `locked`, tidak terdaftar, tabel tanpa resolver.
    for (table, field) in [
        ("stores", "phone"),
        ("stores", "lat"),
        ("stores", "ghost"),
        ("products", "barcode"),
    ] {
        let res = app
            .post(&reveal)
            .token(manager)
            .json(&body(table, field))
            .send()
            .await;
        res.assert_status(403);
        assert_eq!(
            res.text(),
            format!(
                r#"{{"code":"FIELD_NOT_REVEALABLE","message":"This field is not revealable for your role","details":{{"field":"{field}"}}}}"#
            )
        );
    }

    // Konfigurasi: default company phone `revealable`, ownerName `masked`.
    app.put(&format!("{BASE}/matrix"))
        .token(admin)
        .json(&put_body(json!([{ "entries": [
            { "columnKey": "phone", "mode": "revealable" },
            { "columnKey": "ownerName", "mode": "masked" }] }])))
        .send()
        .await
        .assert_status(200);
    // `masked` tidak bisa dibuka.
    app.post(&reveal)
        .token(manager)
        .json(&body("stores", "ownerName"))
        .send()
        .await
        .assert_status(403);
    // Baris tak dikenal → 404 milik tabelnya; kuota tidak terpakai.
    let res = app
        .post(&reveal)
        .token(manager)
        .json(&json!({ "tableKey": "stores", "recordId": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", "field": "phone" }))
        .send()
        .await;
    res.assert_status(404);
    assert_eq!(res.json["code"], "STORE_NOT_FOUND");
    let q: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "fieldRevealQuota" WHERE "companyId" = {}"#,
            c.id
        ))
        .await;
    assert_eq!(q, 0);

    // Sukses: nilai + jejak + kuota.
    let res = app
        .post(&reveal)
        .token(manager)
        .json(&json!({ "tableKey": "stores", "recordId": STORE_UUID, "field": "phone", "reason": "Konfirmasi jadwal" }))
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(
        res.text(),
        r#"{"data":{"field":"phone","value":"0812-000"}}"#
    );
    let (label, actor, role, reason, rid, ip): (
        String,
        i32,
        String,
        String,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(&format!(
        r#"SELECT "recordLabel", "actorId", "actorRole", reason, "requestId"::text, "ipAddress"
                 FROM "fieldRevealLog" WHERE "companyId" = {} AND "columnKey" = 'phone'"#,
        c.id
    ))
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(label, "TK-001 Toko Uji");
    assert_eq!(actor, c.manager.id);
    assert_eq!(role, "MANAGER");
    assert_eq!(reason, "Konfirmasi jadwal");
    assert!(rid.is_some(), "requestId dari scope audit");
    let _ = ip;
    let cnt: i32 = app
        .scalar(&format!(
            r#"SELECT count FROM "fieldRevealQuota" WHERE "companyId" = {} AND "actorId" = {} AND "columnKey" = 'phone'"#,
            c.id, c.manager.id
        ))
        .await;
    assert_eq!(cnt, 1);

    // Kuota habis (50/hari/user/kolom) → 429 dengan badan berurutan; tidak ada jejak baru.
    let day: String = app
        .scalar(&format!(
            r#"SELECT to_char(now() AT TIME ZONE "timeZone", 'YYYY-MM-DD') FROM company WHERE id = {}"#,
            c.id
        ))
        .await;
    app.exec(&format!(
        r#"UPDATE "fieldRevealQuota" SET count = 50 WHERE "companyId" = {} AND "actorId" = {} AND day = '{day}'"#,
        c.id, c.manager.id
    ))
    .await;
    let res = app
        .post(&reveal)
        .token(manager)
        .json(&body("stores", "phone"))
        .send()
        .await;
    res.assert_status(429);
    assert_eq!(
        res.text(),
        r#"{"code":"REVEAL_QUOTA_EXCEEDED","message":"Daily reveal quota exceeded for this column (50 per day).","details":{"field":"phone","quota":50}}"#
    );
    let logs: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "fieldRevealLog" WHERE "companyId" = {}"#,
            c.id
        ))
        .await;
    assert_eq!(logs, 1);
    // Kuota per user: ADMIN masih punya jatah.
    let res = app
        .post(&reveal)
        .token(admin)
        .json(&body("stores", "phone"))
        .send()
        .await;
    res.assert_status(200);

    // Tabel `customers` (fitur pemilik ≠ stores): MANAGER tanpa `customers:READ` ditolak
    // PERMISSION_DENIED; ADMIN yang punya lolos.
    app.put(&format!("{BASE}/matrix"))
        .token(admin)
        .json(&json!({ "tableKey": "customers", "rows": [{ "entries": [
            { "columnKey": "taxNumber", "mode": "revealable" }] }] }))
        .send()
        .await
        .assert_status(200);
    let res = app
        .post(&reveal)
        .token(manager)
        .json(&body("customers", "taxNumber"))
        .send()
        .await;
    res.assert_status(403);
    assert_eq!(
        res.text(),
        r#"{"code":"PERMISSION_DENIED","feature":"customers","action":"READ","message":"Role ini tidak punya izin READ untuk fitur customers"}"#
    );
    let res = app
        .post(&reveal)
        .token(admin)
        .json(&body("customers", "taxNumber"))
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(
        res.text(),
        r#"{"data":{"field":"taxNumber","value":"01.234"}}"#
    );

    // Perubahan kebijakan langsung berlaku di reveal (dibaca segar dari DB).
    app.exec(&format!(
        r#"UPDATE "tableFieldVisibility" SET mode = 'masked' WHERE "companyId" = {} AND "tableKey" = 'stores' AND "columnKey" = 'phone'"#,
        c.id
    ))
    .await;
    app.post(&reveal)
        .token(admin)
        .json(&body("stores", "phone"))
        .send()
        .await
        .assert_status(403);

    // ── reveal-logs.
    let res = app
        .get(&format!("{BASE}/reveal-logs"))
        .token(admin)
        .send()
        .await;
    res.assert_status(200).assert_no_sql_error();
    assert_eq!(
        res.json["meta"],
        json!({ "total": 3, "page": 1, "limit": 20, "totalPages": 1 })
    );
    let first = &res.json["data"][0];
    assert_eq!(first["tableKey"], "customers");
    assert_eq!(first["record"]["label"], "CU-001 Customer Uji");
    assert_eq!(first["actor"]["id"], c.admin.uuid.as_str());
    assert!(first["createdAt"].as_i64().unwrap() > 1_700_000_000_000);
    assert!(res.text().starts_with(r#"{"data":[{"id":""#));
    assert!(
        res.text()
            .contains(r#""record":{"id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","label":"#)
    );
    assert!(
        res.text()
            .contains(r#""meta":{"total":3,"page":1,"limit":20,"totalPages":1}"#)
    );
    // Filter.
    let res = app
        .get(&format!(
            "{BASE}/reveal-logs?tableKey=stores&actorId={}&limit=1",
            c.manager.uuid
        ))
        .token(admin)
        .send()
        .await;
    assert_eq!(
        res.json["meta"],
        json!({ "total": 1, "page": 1, "limit": 1, "totalPages": 1 })
    );
    assert_eq!(res.json["data"][0]["actor"]["role"], "MANAGER");
    // Aktor tak dikenal = nol baris (bukan filter yang gugur).
    let res = app
        .get(&format!(
            "{BASE}/reveal-logs?actorId=11111111-1111-4111-8111-111111111111"
        ))
        .token(admin)
        .send()
        .await;
    assert_eq!(res.json["meta"]["total"], 0);
    assert_eq!(res.json["meta"]["totalPages"], 1);
    // Rentang tanggal (zona company, batas akhir inklusif).
    let res = app
        .get(&format!("{BASE}/reveal-logs?startDate={day}&endDate={day}"))
        .token(admin)
        .send()
        .await;
    assert_eq!(res.json["meta"]["total"], 3);
    let res = app
        .get(&format!(
            "{BASE}/reveal-logs?startDate=2000-01-01&endDate=2000-01-02"
        ))
        .token(admin)
        .send()
        .await;
    assert_eq!(res.json["meta"]["total"], 0);
    // Tanggal bukan tanggal kalender → 400 INVALID_DATE; format salah → pesan DTO.
    let res = app
        .get(&format!("{BASE}/reveal-logs?startDate=2026-02-30"))
        .token(admin)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.text(),
        r#"{"code":"INVALID_DATE","message":"startDate must be a valid YYYY-MM-DD date","details":{"startDate":"2026-02-30"}}"#
    );
    let res = app
        .get(&format!("{BASE}/reveal-logs?startDate=x&page=0&limit=101"))
        .token(admin)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "startDate must be YYYY-MM-DD",
            "page must not be less than 1",
            "limit must not be greater than 100"
        ])
    );
}
