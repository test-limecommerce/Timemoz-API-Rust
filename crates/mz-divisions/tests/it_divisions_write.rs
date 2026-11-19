//! Uji integrasi route tulis `/v1/divisions*` (create, update, ganti peran, keluarkan anggota,
//! hapus) lewat HTTP terhadap Postgres nyata: jalur sukses, 409/422/404, role yang salah, kunci
//! advisory company, dan baris audit.
//! Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-divisions --test it_divisions_write`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::{Duration, Instant};

use axum::response::IntoResponse;
use common::{assert_code, assert_key_order, build, enable};
use http_body_util::BodyExt;
use mz_testkit::{Company, TestApp, TestResponse};
use serde_json::{Value, json};
use tokio::sync::{Mutex, MutexGuard};

/// Semua test memakai company fixture yang sama: dijalankan berurutan.
static SERIAL: Mutex<()> = Mutex::const_new(());

struct Ctx {
    app: TestApp,
    fx: Company,
    _guard: MutexGuard<'static, ()>,
}

async fn setup() -> Option<Ctx> {
    let guard = SERIAL.lock().await;
    let app = TestApp::new(build).await?;
    let fx = app.company().await;
    enable(&app, &fx).await;
    // Pulihkan role/status yang diubah test lain.
    app.exec(&format!(
        r#"UPDATE "userAccessLevel" SET "accessLevelId" = (SELECT id FROM "accessLevel" WHERE code = 'STAFF' AND "isSystem")
             WHERE "userId" IN ({}, {}) AND "companyId" = {} AND status = 1;
           UPDATE users SET status = 1 WHERE id IN ({}, {});"#,
        fx.staff.id, fx.staff2.id, fx.id, fx.staff.id, fx.staff2.id
    ))
    .await;
    Some(Ctx {
        app,
        fx,
        _guard: guard,
    })
}

fn member(uuid: &str, role: &str) -> Value {
    json!({ "userUuid": uuid, "role": role })
}

async fn create(c: &Ctx, name: &str, members: Vec<Value>) -> TestResponse {
    c.app
        .post("/v1/divisions")
        .token(&c.fx.admin.token)
        .json(&json!({ "name": name, "members": members }))
        .send()
        .await
}

async fn create_ok(c: &Ctx, name: &str, members: Vec<Value>) -> (String, Value) {
    let r = create(c, name, members).await;
    r.assert_status(201).assert_no_sql_error();
    (
        r.json["data"]["uuid"].as_str().unwrap().to_owned(),
        r.json["data"].clone(),
    )
}

async fn patch(c: &Ctx, uuid: &str, body: Value) -> TestResponse {
    c.app
        .patch(&format!("/v1/divisions/{uuid}"))
        .token(&c.fx.admin.token)
        .json(&body)
        .send()
        .await
}

async fn live_rows(c: &Ctx, uuid: &str) -> Vec<(i32, String, String)> {
    sqlx::query_as(
        r#"SELECT dm.id, u.uuid::text, dm.role::text FROM "divisionMember" dm
           JOIN division d ON d.id = dm."divisionId" JOIN users u ON u.id = dm."userId"
           WHERE d.uuid = $1::uuid AND dm."deletedAt" IS NULL ORDER BY dm.id"#,
    )
    .bind(uuid)
    .fetch_all(c.app.db())
    .await
    .unwrap()
}

async fn version_of(c: &Ctx, uuid: &str) -> i32 {
    sqlx::query_scalar("SELECT version FROM division WHERE uuid = $1::uuid")
        .bind(uuid)
        .fetch_one(c.app.db())
        .await
        .unwrap()
}

/// Baris audit ditulis di `tokio::spawn` sesudah respons: tunggu sampai jumlahnya cukup.
async fn audit_rows(c: &Ctx, uuid: &str, at_least: usize) -> Vec<(String, Option<String>)> {
    let division_id: i32 = sqlx::query_scalar("SELECT id FROM division WHERE uuid = $1::uuid")
        .bind(uuid)
        .fetch_one(c.app.db())
        .await
        .unwrap();
    for _ in 0..50 {
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            r#"SELECT action::text, "subEntity" FROM "auditLog"
               WHERE "entityType"::text = 'division' AND "entityId" = $1 ORDER BY id"#,
        )
        .bind(division_id)
        .fetch_all(c.app.db())
        .await
        .unwrap();
        if rows.len() >= at_least {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("baris audit < {at_least} untuk divisi {uuid}");
}

#[tokio::test]
async fn create_success_shape_and_audit() {
    let Some(c) = setup().await else { return };
    let fx = &c.fx;
    let r = c
        .app
        .post("/v1/divisions")
        .token(&fx.admin.token)
        .json(&json!({
            "name": "  Eng  ", "description": "  tim  ",
            "members": [member(&fx.staff2.uuid, "MEMBER"), member(&fx.staff.uuid, "LEADER")],
            "confirmMoveUserUuids": [],
        }))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    let d = &r.json["data"];
    assert_eq!(d["name"], json!("Eng"));
    assert_eq!(d["description"], json!("tim"));
    assert_eq!(d["version"], json!(0));
    assert_eq!(d["viewer"], json!({ "canManage": true }));
    assert_eq!(d["leader"]["uuid"], json!(fx.staff.uuid));
    let members = d["members"].as_array().unwrap();
    assert_eq!(members.len(), 2);
    assert_eq!(members[0]["uuid"], json!(fx.staff.uuid)); // Leader dulu
    assert_eq!(members[1]["role"], json!("MEMBER"));
    assert_key_order(
        &r.text(),
        &[
            "uuid",
            "name",
            "description",
            "version",
            "leader",
            "leaders",
            "members",
            "viewer",
        ],
    );

    // DB: Leader dibuat lebih dulu (urutan id), createdById = pembuat.
    let uuid = d["uuid"].as_str().unwrap();
    let rows = live_rows(&c, uuid).await;
    assert_eq!(rows[0].1, fx.staff.uuid);
    assert_eq!(rows[0].2, "LEADER");
    let by: Option<i32> =
        sqlx::query_scalar("SELECT \"createdById\" FROM division WHERE uuid = $1::uuid")
            .bind(uuid)
            .fetch_one(c.app.db())
            .await
            .unwrap();
    assert_eq!(by, Some(fx.admin.id));

    // Audit: satu baris `create` divisi + satu baris keanggotaan (`divisionMember`).
    let audit = audit_rows(&c, uuid, 2).await;
    assert!(audit.contains(&("create".to_owned(), None)), "{audit:?}");
    assert!(
        audit.contains(&("update".to_owned(), Some("divisionMember".to_owned()))),
        "{audit:?}"
    );
    let actor: Option<i32> = sqlx::query_scalar(
        r#"SELECT "actorId" FROM "auditLog" WHERE "entityType"::text = 'division'
           AND "entityUuid" = $1::uuid AND action::text = 'create'"#,
    )
    .bind(uuid)
    .fetch_one(c.app.db())
    .await
    .unwrap();
    assert_eq!(actor, Some(fx.admin.id));
}

#[tokio::test]
async fn create_rolls_back_without_audit_on_failure() {
    let Some(c) = setup().await else { return };
    let fx = &c.app.company().await;
    let before: i64 = c
        .app
        .scalar("SELECT count(*) FROM \"auditLog\" WHERE \"entityType\"::text = 'division'")
        .await;
    // Anggota tak dikenal: gagal di bawah lock, sesudah nama lolos → tidak ada tulisan/audit.
    let r = create(
        &c,
        "Ghost",
        vec![
            member(&fx.staff.uuid, "LEADER"),
            member("3f2504e0-4f89-41d3-9a0c-0305e82c3301", "MEMBER"),
        ],
    )
    .await;
    r.assert_status(422);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let n: i64 = c
        .app
        .scalar("SELECT count(*) FROM division WHERE name = 'Ghost'")
        .await;
    assert_eq!(n, 0);
    let after: i64 = c
        .app
        .scalar("SELECT count(*) FROM \"auditLog\" WHERE \"entityType\"::text = 'division'")
        .await;
    assert_eq!(after, before);
}

#[tokio::test]
async fn create_validation_and_domain_errors() {
    let Some(c) = setup().await else { return };
    let (app, fx) = (&c.app, &c.fx);
    let ok = vec![member(&fx.staff.uuid, "LEADER")];

    // Role salah: RolesGuard (401 `Access denied`).
    for who in [&fx.staff, &fx.manager] {
        let r = app
            .post("/v1/divisions")
            .token(&who.token)
            .json(&json!({ "name": "X", "members": ok }))
            .send()
            .await;
        r.assert_status(401);
        assert_eq!(
            r.json["message"],
            json!("Access denied: insufficient permissions")
        );
    }
    // Super admin boleh menulis.
    let r = app
        .post("/v1/divisions")
        .token(&fx.super_admin.token)
        .json(&json!({ "name": "BySuper", "members": ok }))
        .send()
        .await;
    r.assert_status(201);

    // Validasi body: urutan pesan class-validator.
    let r = app
        .post("/v1/divisions")
        .token(&fx.admin.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "name must be longer than or equal to 1 characters",
            "name must be a string",
            "members must contain no more than 1000 elements",
            "members must be an array"
        ])
    );
    let long = "d".repeat(201);
    let r = app
        .post("/v1/divisions")
        .token(&fx.admin.token)
        .json(&json!({ "name": "X", "description": long, "members": ok }))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!(["description must be shorter than or equal to 200 characters"])
    );
    let r = app
        .post("/v1/divisions")
        .token(&fx.admin.token)
        .json(&json!({ "name": "X", "members": [{ "userUuid": "x", "role": "BOSS" }] }))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!([
            "members.0.userUuid must be a UUID",
            "members.0.role must be one of the following values: LEADER, MEMBER"
        ])
    );

    // Nama unik tanpa membedakan huruf; `_` bukan wildcard.
    create_ok(&c, "Eng", ok.clone()).await;
    let r = create(&c, "eNG", ok.clone()).await;
    assert_code(&r, 409, "DIVISION_NAME_TAKEN");
    assert_eq!(
        r.json["message"],
        json!("A division with this name already exists")
    );
    create_ok(&c, "E_g", ok.clone()).await;
    create_ok(&c, "E%", ok.clone()).await;

    // Minimal satu Leader.
    let r = create(&c, "NoLead", vec![member(&fx.staff2.uuid, "MEMBER")]).await;
    assert_code(&r, 422, "DIVISION_LEADER_REQUIRED");
    assert_eq!(
        r.json["message"],
        json!("A division needs at least one Leader")
    );

    // Duplikat (uuid beda huruf) menang atas hitungan Leader; urutan kunci Node: details terakhir.
    let r = create(
        &c,
        "Dup",
        vec![
            member(&fx.staff.uuid, "LEADER"),
            member(&fx.staff.uuid.to_uppercase(), "MEMBER"),
        ],
    )
    .await;
    r.assert_status(422);
    assert_eq!(
        r.text(),
        format!(
            r#"{{"code":"DIVISION_DUPLICATE_MEMBER","message":"The same employee is listed more than once","details":{{"userUuids":["{}"]}}}}"#,
            fx.staff.uuid
        )
    );

    // Bukan karyawan aktif company (1 dan 2 uuid; urutan masukan).
    let (u1, u2) = (
        "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
        "9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d",
    );
    let r = create(
        &c,
        "Out",
        vec![
            member(&fx.staff.uuid, "LEADER"),
            member(u2, "MEMBER"),
            member(u1, "MEMBER"),
        ],
    )
    .await;
    r.assert_status(422);
    assert_eq!(
        r.text(),
        format!(
            r#"{{"code":"DIVISION_MEMBER_NOT_IN_COMPANY","message":"2 employees are not active employees of this company","details":{{"userUuids":["{u2}","{u1}"]}}}}"#
        )
    );
    let r = create(
        &c,
        "Out",
        vec![member(&fx.staff.uuid, "LEADER"), member(u1, "MEMBER")],
    )
    .await;
    assert_eq!(
        r.json["message"],
        json!("1 employee is not an active employee of this company")
    );
    // Karyawan nonaktif (users.status = 3) juga ditolak.
    app.exec(&format!(
        "UPDATE users SET status = 3 WHERE id = {}",
        fx.staff2.id
    ))
    .await;
    let r = create(
        &c,
        "Out",
        vec![
            member(&fx.staff.uuid, "LEADER"),
            member(&fx.staff2.uuid, "MEMBER"),
        ],
    )
    .await;
    assert_code(&r, 422, "DIVISION_MEMBER_NOT_IN_COMPANY");
    app.exec(&format!(
        "UPDATE users SET status = 1 WHERE id = {}",
        fx.staff2.id
    ))
    .await;

    // Guest tidak boleh jadi Leader; details = daftar (userUuid, name) dalam urutan Node.
    app.exec(&format!(
        r#"UPDATE "userAccessLevel" SET "accessLevelId" = (SELECT id FROM "accessLevel" WHERE code = 'GUEST' AND "isSystem")
             WHERE "userId" = {} AND "companyId" = {} AND status = 1"#,
        fx.staff2.id, fx.id
    ))
    .await;
    let r = create(&c, "G", vec![member(&fx.staff2.uuid, "LEADER")]).await;
    r.assert_status(422);
    let name: String = app
        .scalar(&format!(
            "SELECT name FROM users WHERE id = {}",
            fx.staff2.id
        ))
        .await;
    assert_eq!(
        r.text(),
        format!(
            r#"{{"code":"DIVISION_LEADER_NOT_ELIGIBLE","message":"{name} cannot be a Leader because their role is Guest","details":[{{"userUuid":"{}","name":"{name}"}}]}}"#,
            fx.staff2.uuid
        )
    );
    // Guest sebagai Member biasa boleh.
    create_ok(
        &c,
        "GuestMember",
        vec![
            member(&fx.staff.uuid, "LEADER"),
            member(&fx.staff2.uuid, "MEMBER"),
        ],
    )
    .await;

    // Tidak ada divisi yang bocor dari percobaan gagal.
    let n: i64 = app
        .scalar("SELECT count(*) FROM division WHERE name IN ('Dup', 'Out', 'G', 'NoLead') AND \"deletedAt\" IS NULL")
        .await;
    assert_eq!(n, 0);
}

#[tokio::test]
async fn company_lock_serialises_writes_with_exact_key() {
    let Some(c) = setup().await else { return };
    let fx = &c.fx;
    // Pegang `pg_advisory_xact_lock(hashtext('divisions:company:{id}'))` di koneksi lain: tulis
    // divisi harus menunggu; baca tidak.
    let mut tx = c.app.db().begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("divisions:company:{}", fx.id))
        .execute(&mut *tx)
        .await
        .unwrap();
    let start = Instant::now();
    let write = async {
        let r = create(&c, "Locked", vec![member(&fx.staff.uuid, "LEADER")]).await;
        (r, start.elapsed())
    };
    let release = async {
        tokio::time::sleep(Duration::from_millis(900)).await;
        tx.commit().await.unwrap();
    };
    let read = async {
        let t = Instant::now();
        let r = c
            .app
            .get("/v1/divisions")
            .token(&fx.admin.token)
            .send()
            .await;
        (r, t.elapsed())
    };
    let ((r, took), (read_res, read_took), ()) = tokio::join!(write, read, release);
    r.assert_status(201);
    assert!(
        took >= Duration::from_millis(800),
        "tulis tidak menunggu lock: {took:?}"
    );
    read_res.assert_status(200);
    assert!(
        read_took < Duration::from_millis(800),
        "baca ikut terkunci: {read_took:?}"
    );
}

#[tokio::test]
async fn update_flow() {
    let Some(c) = setup().await else { return };
    let (app, fx) = (&c.app, &c.fx);
    let (uuid, d) = create_ok(
        &c,
        "Plat",
        vec![
            member(&fx.staff.uuid, "LEADER"),
            member(&fx.staff2.uuid, "MEMBER"),
        ],
    )
    .await;
    assert_eq!(d["version"], json!(0));
    let before = live_rows(&c, &uuid).await;

    // Validasi body + peran.
    let r = patch(&c, &uuid, json!({})).await;
    assert_eq!(
        r.json["message"],
        json!([
            "version must not be less than 0",
            "version must be an integer number"
        ])
    );
    for who in [&fx.manager, &fx.staff] {
        app.patch(&format!("/v1/divisions/{uuid}"))
            .token(&who.token)
            .json(&json!({ "version": 0 }))
            .send()
            .await
            .assert_status(401);
    }

    // Nama saja: version naik, anggota tak disentuh (id baris sama).
    let r = patch(
        &c,
        &uuid,
        json!({ "version": 0, "name": " Plat2 ", "description": "x" }),
    )
    .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["data"]["name"], json!("Plat2"));
    assert_eq!(r.json["data"]["description"], json!("x"));
    assert_eq!(r.json["data"]["version"], json!(1));
    assert_eq!(live_rows(&c, &uuid).await, before);
    // Description `null` / spasi saja mengosongkan.
    let r = patch(&c, &uuid, json!({ "version": 1, "description": "   " })).await;
    assert_eq!(r.json["data"]["description"], Value::Null);
    assert_eq!(r.json["data"]["version"], json!(2));
    let r = patch(&c, &uuid, json!({ "version": 2, "description": "y" })).await;
    r.assert_status(200);
    let r = patch(&c, &uuid, json!({ "version": 3, "description": null })).await;
    assert_eq!(r.json["data"]["description"], Value::Null);
    // PATCH dengan isi identik tetap menaikkan version.
    let r = patch(&c, &uuid, json!({ "version": 4, "name": "plat2" })).await;
    assert_eq!(r.json["data"]["version"], json!(5));
    assert_eq!(r.json["data"]["name"], json!("plat2"));

    // Stale.
    let r = patch(&c, &uuid, json!({ "version": 0, "name": "Z" })).await;
    assert_code(&r, 409, "DIVISION_STALE");
    assert_eq!(
        r.json["message"],
        json!("This division was changed by someone else; reload and try again")
    );
    // 404: uuid tak ada / bentuk valid tapi bukan isUUID; 400: bukan uuid.
    for u in [
        "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
        "12345678-1234-1234-1234-123456789012",
    ] {
        assert_code(
            &patch(&c, u, json!({ "version": 0 })).await,
            404,
            "DIVISION_NOT_FOUND",
        );
    }
    assert_eq!(
        patch(&c, "nope", json!({ "version": 0 })).await.json["message"],
        json!("Validation failed (uuid is expected)")
    );

    // Nama bentrok dengan divisi lain.
    create_ok(&c, "Other", vec![member(&fx.manager.uuid, "LEADER")]).await;
    let r = patch(&c, &uuid, json!({ "version": 5, "name": "OTHER" })).await;
    assert_code(&r, 409, "DIVISION_NAME_TAKEN");
    assert_eq!(version_of(&c, &uuid).await, 5);

    // Ganti seluruh anggota: staff keluar, manager jadi Leader, staff2 tetap (baris tidak disentuh).
    let before = live_rows(&c, &uuid).await;
    let staff2_row = before
        .iter()
        .find(|r| r.1 == fx.staff2.uuid)
        .unwrap()
        .clone();
    let r = patch(
        &c,
        &uuid,
        json!({ "version": 5, "members": [member(&fx.staff2.uuid, "MEMBER"), member(&fx.manager.uuid, "LEADER")] }),
    )
    .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["data"]["version"], json!(6));
    assert_eq!(r.json["data"]["leader"]["uuid"], json!(fx.manager.uuid));
    let after = live_rows(&c, &uuid).await;
    assert_eq!(after.len(), 2);
    assert!(after.contains(&staff2_row), "baris staff2 harus utuh");
    assert!(!after.iter().any(|r| r.1 == fx.staff.uuid));
    // Baris lama staff di-soft-delete, bukan dihapus.
    let gone: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "divisionMember" dm JOIN division d ON d.id = dm."divisionId"
               WHERE d.uuid = '{uuid}' AND dm."userId" = {} AND dm."deletedAt" IS NOT NULL"#,
            fx.staff.id
        ))
        .await;
    assert_eq!(gone, 1);

    // Ganti peran lewat diff: staff2 MEMBER → LEADER = cabut + buat ulang (id baru).
    let r = patch(
        &c,
        &uuid,
        json!({ "version": 6, "members": [member(&fx.staff2.uuid, "LEADER"), member(&fx.manager.uuid, "LEADER")] }),
    )
    .await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["leaders"].as_array().unwrap().len(), 2);
    let after2 = live_rows(&c, &uuid).await;
    let s2 = after2.iter().find(|r| r.1 == fx.staff2.uuid).unwrap();
    assert_ne!(s2.0, staff2_row.0);
    assert_eq!(s2.2, "LEADER");

    // Validasi anggota (422) tidak menulis apa pun dan tidak menaikkan version.
    let v = version_of(&c, &uuid).await;
    let r = patch(
        &c,
        &uuid,
        json!({ "version": v, "members": [member(&fx.staff2.uuid, "MEMBER")] }),
    )
    .await;
    assert_code(&r, 422, "DIVISION_LEADER_REQUIRED");
    let r = patch(&c, &uuid, json!({ "version": v, "members": [] })).await;
    assert_code(&r, 422, "DIVISION_LEADER_REQUIRED");
    assert_eq!(version_of(&c, &uuid).await, v);
    assert_eq!(live_rows(&c, &uuid).await.len(), 2);

    // Tanpa `members`: divisi wajib sudah punya Leader sah. Jadikan semua Leader Guest.
    app.exec(&format!(
        r#"UPDATE "userAccessLevel" SET "accessLevelId" = (SELECT id FROM "accessLevel" WHERE code = 'GUEST' AND "isSystem")
             WHERE "userId" = {} AND "companyId" = {} AND status = 1"#,
        fx.staff2.id, fx.id
    ))
    .await;
    app.exec(&format!(
        r#"UPDATE "divisionMember" SET role = 'MEMBER' WHERE "userId" = {} AND "deletedAt" IS NULL"#,
        fx.manager.id
    ))
    .await;
    let r = patch(&c, &uuid, json!({ "version": v, "name": "Plat3" })).await;
    assert_code(&r, 422, "DIVISION_LEADER_REQUIRED");
    assert_eq!(
        r.json["message"],
        json!("This division has no Leader; choose a Leader before saving")
    );

    // Audit: ada baris `update` untuk divisi ini.
    let audit = audit_rows(&c, &uuid, 3).await;
    assert!(audit.iter().any(|a| a.0 == "update"), "{audit:?}");
}

#[tokio::test]
async fn member_role_and_remove_member() {
    let Some(c) = setup().await else { return };
    let (app, fx) = (&c.app, &c.fx);
    let (uuid, _) = create_ok(
        &c,
        "Roles",
        vec![
            member(&fx.staff.uuid, "LEADER"),
            member(&fx.staff2.uuid, "MEMBER"),
            member(&fx.manager.uuid, "MEMBER"),
        ],
    )
    .await;
    let url = |u: &str| format!("/v1/divisions/{uuid}/members/{u}");
    let set = |u: &str, role: &str, version: i64| {
        app.patch(&url(u))
            .token(&fx.admin.token)
            .json(&json!({ "role": role, "version": version }))
            .send()
    };

    // Validasi body dan peran.
    let r = app
        .patch(&url(&fx.staff2.uuid))
        .token(&fx.admin.token)
        .json(&json!({ "role": "X" }))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!([
            "role must be one of the following values: LEADER, MEMBER",
            "version must not be less than 0",
            "version must be an integer number"
        ])
    );
    for who in [&fx.manager, &fx.staff] {
        app.patch(&url(&fx.staff2.uuid))
            .token(&who.token)
            .json(&json!({ "role": "LEADER", "version": 0 }))
            .send()
            .await
            .assert_status(401);
    }
    // Param uuid diperiksa berurutan (uuid, lalu userUuid), sebelum body.
    let r = app
        .patch("/v1/divisions/nope/members/also-nope")
        .token(&fx.admin.token)
        .json(&json!({}))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!("Validation failed (uuid is expected)")
    );

    // Promosi: Leader lain tetap Leader.
    let r = set(&fx.staff2.uuid, "LEADER", 0).await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["data"]["version"], json!(1));
    assert_eq!(r.json["data"]["leaders"].as_array().unwrap().len(), 2);
    // No-op: peran sama, version TIDAK naik, tapi cek version tetap berlaku.
    let r = set(&fx.staff2.uuid, "LEADER", 1).await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["version"], json!(1));
    assert_eq!(version_of(&c, &uuid).await, 1);
    assert_code(
        &set(&fx.staff2.uuid, "LEADER", 0).await,
        409,
        "DIVISION_STALE",
    );
    // Turunkan Leader (masih ada yang lain).
    let r = set(&fx.staff.uuid, "MEMBER", 1).await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["version"], json!(2));
    assert_eq!(r.json["data"]["leaders"].as_array().unwrap().len(), 1);
    // Leader terakhir tidak boleh turun.
    let r = set(&fx.staff2.uuid, "MEMBER", 2).await;
    assert_code(&r, 422, "DIVISION_LEADER_REQUIRED");
    assert_eq!(
        r.json["message"],
        json!("Appoint another Leader before demoting the last one")
    );
    assert_eq!(version_of(&c, &uuid).await, 2);
    // Guest tidak boleh dipromosikan.
    app.exec(&format!(
        r#"UPDATE "userAccessLevel" SET "accessLevelId" = (SELECT id FROM "accessLevel" WHERE code = 'GUEST' AND "isSystem")
             WHERE "userId" = {} AND "companyId" = {} AND status = 1"#,
        fx.staff.id, fx.id
    ))
    .await;
    let r = set(&fx.staff.uuid, "LEADER", 2).await;
    assert_code(&r, 422, "DIVISION_LEADER_NOT_ELIGIBLE");
    app.exec(&format!(
        r#"UPDATE "userAccessLevel" SET "accessLevelId" = (SELECT id FROM "accessLevel" WHERE code = 'STAFF' AND "isSystem")
             WHERE "userId" = {} AND "companyId" = {} AND status = 1"#,
        fx.staff.id, fx.id
    ))
    .await;
    // Bukan anggota → 404 DIVISION_MEMBER_NOT_FOUND; uuid bentuk-valid-bukan-isUUID juga 404.
    for u in [
        fx.admin.uuid.as_str(),
        "12345678-1234-1234-1234-123456789012",
    ] {
        let r = set(u, "LEADER", 2).await;
        assert_code(&r, 404, "DIVISION_MEMBER_NOT_FOUND");
        assert_eq!(
            r.json["message"],
            json!("This employee is not a member of the division")
        );
    }
    // Divisi tak ada → 404 DIVISION_NOT_FOUND (sebelum anggota).
    let r = app
        .patch(&format!(
            "/v1/divisions/3f2504e0-4f89-41d3-9a0c-0305e82c3301/members/{}",
            fx.staff.uuid
        ))
        .token(&fx.admin.token)
        .json(&json!({ "role": "LEADER", "version": 0 }))
        .send()
        .await;
    assert_code(&r, 404, "DIVISION_NOT_FOUND");

    // ── DELETE members ──
    let del = |u: &str, q: &str| {
        app.delete(&format!("{}{q}", url(u)))
            .token(&fx.admin.token)
            .send()
    };
    let r = del(&fx.manager.uuid, "").await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "version must not be less than 0",
            "version must be an integer number"
        ])
    );
    assert_eq!(
        del(&fx.manager.uuid, "?version=abc").await.json["message"],
        json!([
            "version must not be less than 0",
            "version must be an integer number"
        ])
    );
    for who in [&fx.manager, &fx.staff] {
        app.delete(&format!("{}?version=2", url(&fx.manager.uuid)))
            .token(&who.token)
            .send()
            .await
            .assert_status(401);
    }
    assert_code(
        &del(&fx.manager.uuid, "?version=1").await,
        409,
        "DIVISION_STALE",
    );
    let r = del(&fx.manager.uuid, "?version=2").await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["data"]["version"], json!(3));
    assert_eq!(r.json["data"]["members"].as_array().unwrap().len(), 2);
    // Anggota biasa dicabut tanpa menyentuh data karyawan / divisi lain (baris soft-deleted).
    let gone: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "divisionMember" dm JOIN division d ON d.id = dm."divisionId"
               WHERE d.uuid = '{uuid}' AND dm."userId" = {} AND dm."deletedAt" IS NOT NULL"#,
            fx.manager.id
        ))
        .await;
    assert_eq!(gone, 1);
    assert_code(
        &del(&fx.manager.uuid, "?version=3").await,
        404,
        "DIVISION_MEMBER_NOT_FOUND",
    );
    // Leader terakhir tidak boleh keluar.
    let r = del(&fx.staff2.uuid, "?version=3").await;
    assert_code(&r, 422, "DIVISION_LEADER_REQUIRED");
    assert_eq!(
        r.json["message"],
        json!("Appoint another Leader before removing the last one")
    );
    // Member boleh keluar (staff), lalu version naik lagi.
    let r = del(&fx.staff.uuid, "?version=3").await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["version"], json!(4));
    assert_eq!(live_rows(&c, &uuid).await.len(), 1);
}

#[tokio::test]
async fn delete_division_soft_deletes_everything_and_frees_name() {
    let Some(c) = setup().await else { return };
    let (app, fx) = (&c.app, &c.fx);
    let (uuid, _) = create_ok(
        &c,
        "Gone",
        vec![
            member(&fx.manager.uuid, "LEADER"),
            member(&fx.staff.uuid, "LEADER"),
            member(&fx.staff2.uuid, "MEMBER"),
        ],
    )
    .await;
    // Satu baris dorman (user nonaktif) ikut di-soft-delete tetapi tidak dihitung.
    app.exec(&format!(
        r#"INSERT INTO "divisionMember" ("companyId", "divisionId", "userId", role)
             SELECT {}, d.id, {}, 'MEMBER'::"DivisionMemberRole" FROM division d WHERE d.uuid = '{uuid}';
           UPDATE users SET status = 3 WHERE id = {};"#,
        fx.id, fx.admin.id, fx.admin.id
    ))
    .await;

    let del = |q: &str| {
        app.delete(&format!("/v1/divisions/{uuid}{q}"))
            .token(&fx.admin.token)
            .send()
    };
    for who in [&fx.manager, &fx.staff] {
        app.delete(&format!("/v1/divisions/{uuid}?version=0"))
            .token(&who.token)
            .send()
            .await
            .assert_status(401);
    }
    assert_eq!(del("").await.status.as_u16(), 400);
    assert_code(&del("?version=7").await, 409, "DIVISION_STALE");

    let r = del("?version=0").await;
    r.assert_status(200).assert_no_sql_error();
    assert_key_order(
        &r.text(),
        &["data", "uuid", "removedMembers", "leader", "leaders"],
    );
    let d = &r.json["data"];
    assert_eq!(d["uuid"], json!(uuid));
    assert_eq!(d["removedMembers"], json!(3)); // dorman tidak dihitung
    let leaders = d["leaders"].as_array().unwrap();
    assert_eq!(leaders.len(), 2);
    assert_eq!(d["leader"], leaders[0]);
    // Leader urut nama (localeCompare), lalu id.
    let names: Vec<String> = leaders
        .iter()
        .map(|l| l["name"].as_str().unwrap().to_lowercase())
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);

    // DB: divisi + SEMUA baris (termasuk dorman) ter-soft-delete, version naik, data karyawan utuh.
    let (deleted, version): (bool, i32) = sqlx::query_as(
        r#"SELECT "deletedAt" IS NOT NULL, version FROM division WHERE uuid = $1::uuid"#,
    )
    .bind(&uuid)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert!(deleted);
    assert_eq!(version, 1);
    let live: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "divisionMember" dm JOIN division d ON d.id = dm."divisionId"
               WHERE d.uuid = '{uuid}' AND dm."deletedAt" IS NULL"#
        ))
        .await;
    assert_eq!(live, 0);
    let users: i64 = app
        .scalar(&format!(
            "SELECT count(*) FROM users WHERE id IN ({}, {}, {})",
            fx.manager.id, fx.staff.id, fx.staff2.id
        ))
        .await;
    assert_eq!(users, 3);

    // Sesudahnya: 404; nama bebas dipakai lagi; Leader kehilangan jangkauan.
    assert_code(&del("?version=1").await, 404, "DIVISION_NOT_FOUND");
    assert_code(
        &app.get(&format!("/v1/divisions/{uuid}"))
            .token(&fx.admin.token)
            .send()
            .await,
        404,
        "DIVISION_NOT_FOUND",
    );
    create_ok(&c, "Gone", vec![member(&fx.manager.uuid, "LEADER")]).await;
    assert_code(
        &app.get("/v1/divisions").token(&fx.staff.token).send().await,
        403,
        "DIVISION_NOT_LEADER",
    );
    // Audit: pembaruan divisi tercatat.
    let audit = audit_rows(&c, &uuid, 2).await;
    assert!(audit.iter().any(|a| a.0 == "create"), "{audit:?}");
    app.exec(&format!(
        "UPDATE users SET status = 1 WHERE id = {}",
        fx.admin.id
    ))
    .await;
}

/// Backstop unique-violation (T4/T9) yang lolos dari pemeriksaan di bawah lock: dipetakan ke
/// 409 DIVISION_NAME_TAKEN / DIVISION_STALE; unique lain tidak dipetakan.
#[tokio::test]
async fn unique_violations_map_like_node() {
    let Some(c) = setup().await else { return };
    let (app, fx) = (&c.app, &c.fx);
    let (uuid, _) = create_ok(&c, "Uniq", vec![member(&fx.staff.uuid, "LEADER")]).await;

    async fn body_of(e: mz_divisions::DivError) -> (u16, String) {
        let res = e.into_response();
        let status = res.status().as_u16();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    let dup_name = sqlx::query(r#"INSERT INTO division (name, "companyId") VALUES ('UNIQ', $1)"#)
        .bind(fx.id)
        .execute(app.db())
        .await
        .unwrap_err();
    let (status, body) = body_of(mz_divisions::map_unique_violation(&dup_name).unwrap()).await;
    assert_eq!(status, 409);
    assert!(body.contains("DIVISION_NAME_TAKEN"), "{body}");

    let dup_member = sqlx::query(
        r#"INSERT INTO "divisionMember" ("companyId", "divisionId", "userId", role)
           SELECT $1, d.id, $2, 'MEMBER'::"DivisionMemberRole" FROM division d WHERE d.uuid = $3::uuid"#,
    )
    .bind(fx.id)
    .bind(fx.staff.id)
    .bind(&uuid)
    .execute(app.db())
    .await
    .unwrap_err();
    let (status, body) = body_of(mz_divisions::map_unique_violation(&dup_member).unwrap()).await;
    assert_eq!(status, 409);
    assert_eq!(
        body,
        r#"{"code":"DIVISION_STALE","message":"Division data changed at the same time; reload and try again"}"#
    );

    // Unique lain (uuid) tidak dipetakan.
    let dup_uuid = sqlx::query(
        r#"INSERT INTO division (uuid, name, "companyId") SELECT uuid, 'Other-name', "companyId" FROM division WHERE uuid = $1::uuid"#,
    )
    .bind(&uuid)
    .execute(app.db())
    .await
    .unwrap_err();
    assert!(mz_divisions::map_unique_violation(&dup_uuid).is_none());
}
