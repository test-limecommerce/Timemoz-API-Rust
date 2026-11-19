//! Uji integrasi 15 route `mz-roles` lewat HTTP (gerbang lengkap + layer audit) terhadap
//! Postgres nyata: jalur sukses tiap route, role salah, eskalasi, anti-lockout, override bawaan,
//! dan invalidasi cache Rust sesudah matriks berubah.
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-roles --test it_roles_api`.

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
use tokio::sync::Mutex;

/// Semua test memakai company fixture yang sama: berurutan, state dipulihkan.
static SERIAL: Mutex<()> = Mutex::const_new(());

const UUID_PIPE: &str =
    r#"{"message":"Validation failed (uuid is expected)","error":"Bad Request","statusCode":400}"#;

async fn not_found(uri: axum::http::Uri, method: axum::http::Method) -> mz_core::ApiError {
    mz_core::ApiError::not_found(format!("Cannot {method} {}", uri.path()))
}

fn build(state: &AppState) -> Router {
    mz_roles::router(state)
        .fallback(not_found)
        .method_not_allowed_fallback(not_found)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

async fn setup() -> Option<(TestApp, Company)> {
    // Channel unik: jangan pernah mengganggu Node lokal di `mz:cache-invalidate`.
    let channel = format!("mz:test:roles:{}", uuid::Uuid::new_v4().simple());
    let app = TestApp::with_vars(build, &[("CACHE_INVALIDATION_CHANNEL", &channel)]).await?;
    let c = app.company().await;
    Some((app, c))
}

fn ok(res: &TestResponse) -> &TestResponse {
    res.assert_no_sql_error();
    res.assert_ok();
    res
}

async fn builtin(app: &TestApp, code: &str) -> (i32, String) {
    sqlx::query_as(r#"SELECT id, uuid::text FROM "accessLevel" WHERE code = $1 AND "isSystem""#)
        .bind(code)
        .fetch_one(app.db())
        .await
        .unwrap()
}

async fn role_id(app: &TestApp, uuid: &str) -> i32 {
    sqlx::query_scalar(r#"SELECT id FROM "accessLevel" WHERE uuid = $1::uuid"#)
        .bind(uuid)
        .fetch_one(app.db())
        .await
        .unwrap()
}

/// `accessLevelId` aktif user di company fixture.
async fn ual_of(app: &TestApp, c: &Company, user_id: i32) -> i32 {
    sqlx::query_scalar(
        r#"SELECT "accessLevelId" FROM "userAccessLevel" WHERE "userId" = $1 AND "companyId" = $2 AND status = 1"#,
    )
    .bind(user_id)
    .bind(c.id)
    .fetch_one(app.db())
    .await
    .unwrap()
}

async fn set_ual(app: &TestApp, c: &Company, user_id: i32, access_level_id: i32) {
    sqlx::query(
        r#"UPDATE "userAccessLevel" SET "accessLevelId" = $3 WHERE "userId" = $1 AND "companyId" = $2 AND status = 1"#,
    )
    .bind(user_id)
    .bind(c.id)
    .bind(access_level_id)
    .execute(app.db())
    .await
    .unwrap();
    // Tulisan SQL langsung di luar API: bersihkan cache seperti Node `invalidate(userId)`.
    app.state.caches.invalidate_user_access_local(user_id);
}

/// Role custom baru lewat API (sebagai ADMIN fixture).
async fn create_role(app: &TestApp, c: &Company, body: Value) -> Value {
    let res = app
        .post("/v1/admin/roles")
        .token(&c.admin.token)
        .json(&body)
        .send()
        .await;
    res.assert_status(201);
    ok(&res);
    res.data().clone()
}

fn rwd(feature: &str) -> Value {
    json!({ "featureKey": feature, "actions": ["READ", "WRITE", "DELETE"] })
}

async fn wait_audit(app: &TestApp, sql: &str) -> i64 {
    for _ in 0..50 {
        let n: i64 = app.scalar(sql).await;
        if n > 0 {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    0
}

#[tokio::test]
async fn features_templates_and_wrong_roles() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };

    let res = app
        .get("/v1/admin/permissions/features")
        .token(&c.admin.token)
        .send()
        .await;
    ok(&res);
    assert_eq!(
        res.text(),
        include_str!("golden/features.json").trim_end(),
        "harus byte-identik dengan JSON.stringify Node"
    );
    let res = app
        .get("/v1/admin/permissions/templates")
        .token(&c.super_admin.token)
        .send()
        .await;
    ok(&res);
    assert_eq!(res.text(), include_str!("golden/templates.json").trim_end());

    // Role bawaan di bawah plafon: 401 lama (bukan 403) untuk keenam jabatan bawaan.
    for (path, user) in [
        ("/v1/admin/permissions/features", &c.staff),
        ("/v1/admin/roles", &c.manager),
        ("/v1/admin/roles", &c.staff),
        ("/v1/roles/assignable", &c.staff),
    ] {
        let res = app.get(path).token(&user.token).send().await;
        res.assert_status(401);
        assert_eq!(
            res.json,
            json!({"message":"Access denied: insufficient permissions","error":"Unauthorized","statusCode":401})
        );
    }
    // Tanpa token.
    app.get("/v1/me/permissions")
        .send()
        .await
        .assert_status(401);
    app.get("/v1/admin/roles").send().await.assert_status(401);
}

#[tokio::test]
async fn me_permissions_and_assignable() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };

    let res = app
        .get("/v1/me/permissions")
        .token(&c.staff.token)
        .send()
        .await;
    ok(&res);
    let d = res.data();
    assert_eq!(d["dataScope"], "SELF");
    assert_eq!(d["scopeDescription"], "Hanya melihat datanya sendiri.");
    assert_eq!(d["enforced"], true);
    let features = d["features"].as_array().unwrap();
    assert_eq!(features.len(), 42);
    let users = features.iter().find(|f| f["key"] == "users").unwrap();
    assert_eq!(
        users,
        &json!({"key":"users","label":"Karyawan","labelEn":"Users","group":"Data Management",
                "surfaces":["web","mobile"],"personScoped":false,"actions":["READ","DELETE"]})
    );
    // legacyOpen: role bawaan selalu penuh.
    let shots = features.iter().find(|f| f["key"] == "screenshots").unwrap();
    assert_eq!(shots["actions"], json!(["READ", "WRITE", "DELETE"]));

    let res = app
        .get("/v1/me/permissions")
        .token(&c.admin.token)
        .send()
        .await;
    ok(&res);
    assert_eq!(res.data()["dataScope"], "COMPANY");

    // Assignable: MANAGER boleh membuka; baris di atas jabatannya tetap ada, `assignable:false`.
    let res = app
        .get("/v1/roles/assignable")
        .token(&c.manager.token)
        .send()
        .await;
    ok(&res);
    let rows = res.data().as_array().unwrap().clone();
    let slots: Vec<&str> = rows
        .iter()
        .filter_map(|r| r["builtInSlot"].as_str())
        .collect();
    assert_eq!(slots, ["ADMIN", "MANAGER", "STAFF", "GUEST"]);
    assert!(
        rows.iter()
            .all(|r| r["code"] != "SUPER_ADMIN" && r["code"] != "SYSTEM_ADMIN")
    );
    assert_eq!(rows[0]["assignable"], false);
    assert_eq!(rows[1]["assignable"], true);
    assert_eq!(
        rows[0].as_object().unwrap().keys().count(),
        9,
        "tanpa status/createdAt/updatedAt"
    );
    let res = app
        .get("/v1/roles/assignable")
        .token(&c.admin.token)
        .send()
        .await;
    ok(&res);
    assert_eq!(res.data()[0]["assignable"], true);
}

#[tokio::test]
async fn crud_flow_and_validation() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let tag = &uuid::Uuid::new_v4().simple().to_string()[..6];
    let name = format!("IT Supervisor {tag}");

    // Validasi (ValidationPipe) & penolakan service.
    let res = app
        .post("/v1/admin/roles")
        .token(&c.admin.token)
        .json(&json!({}))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "name must be shorter than or equal to 255 characters",
            "name should not be empty",
            "name must be a string",
            "baseRole must be one of the following values: ADMIN, MANAGER, STAFF"
        ])
    );
    let res = app
        .post("/v1/admin/roles")
        .token(&c.admin.token)
        .json(&json!({"name": "x", "baseRole": "STAFF", "code": "c2_hack"}))
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(
        res.text(),
        r#"{"code":"CODE_IS_SERVER_GENERATED","message":"'code' is server-generated and cannot be set by the client"}"#
    );
    let res = app
        .post("/v1/admin/roles")
        .token(&c.admin.token)
        .json(&json!({"name": "x", "baseRole": "STAFF", "permissions": [
            {"featureKey": "store-visits", "actions": ["READ"]}]}))
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(
        res.text(),
        r#"{"code":"MISSING_DEPENDENCY","feature":"store-visits","requires":["stores"],"message":"Feature 'Kunjungan Toko' requires 'Toko' to also be checked"}"#
    );
    let res = app
        .post("/v1/admin/roles")
        .token(&c.admin.token)
        .json(&json!({"name": "x", "baseRole": "STAFF", "permissions": [
            {"featureKey": "access-levels", "actions": ["READ"]}]}))
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(res.json["code"], "SYSTEM_ONLY_FEATURE");
    let res = app
        .post("/v1/admin/roles")
        .token(&c.admin.token)
        .json(&json!({"name": "x", "baseRole": "STAFF", "permissions": [
            {"featureKey": "clock", "actions": ["DELETE"]}]}))
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(
        res.text(),
        r#"{"code":"ACTION_NOT_SUPPORTED","feature":"clock","actions":["DELETE"],"message":"Feature 'Absensi' does not support DELETE"}"#
    );

    // Create (201) — matriks dinormalisasi (§5.4), code digenerate server-side.
    let created = create_role(
        &app,
        &c,
        json!({"name": name, "description": "Tim IT", "baseRole": "MANAGER", "permissions": [
            {"featureKey": "users", "actions": ["READ"]},
            {"featureKey": "store-visits", "actions": ["WRITE"]},
            {"featureKey": "stores", "actions": ["READ"]}
        ]}),
    )
    .await;
    let uuid = created["uuid"].as_str().unwrap().to_owned();
    let code = format!("c{}_it_supervisor_{tag}", c.id);
    assert_eq!(created["code"], code.as_str());
    assert_eq!(created["isSystem"], false);
    assert_eq!(created["isBuiltInOverride"], false);
    assert_eq!(created["builtInSlot"], Value::Null);
    assert_eq!(
        created["permissions"],
        json!([
            {"featureKey": "users", "actions": ["READ"]},
            {"featureKey": "store-visits", "actions": ["WRITE", "READ"]},
            {"featureKey": "stores", "actions": ["READ"]}
        ])
    );
    let keys: Vec<&String> = created.as_object().unwrap().keys().collect();
    assert_eq!(keys.len(), 12);
    let id = role_id(&app, &uuid).await;
    assert!(
        wait_audit(
            &app,
            &format!(
                r#"SELECT COUNT(*) FROM "auditLog" WHERE "entityType" = 'role' AND "entityId" = {id} AND action = 'create'"#
            )
        )
        .await
            > 0,
        "accessLevel.create harus diaudit"
    );

    // Nama sama → sufiks `_2` (generateRoleCode).
    let dup = create_role(&app, &c, json!({"name": name, "baseRole": "STAFF"})).await;
    assert_eq!(dup["code"], format!("{code}_2").as_str());

    // Detail, list, impact.
    let res = app
        .get(&format!("/v1/admin/roles/{uuid}"))
        .token(&c.admin.token)
        .send()
        .await;
    ok(&res);
    assert_eq!(res.data(), &created);
    let res = app
        .get("/v1/admin/roles")
        .token(&c.admin.token)
        .send()
        .await;
    ok(&res);
    let list = res.data().as_array().unwrap();
    assert!(list.iter().any(|r| r["uuid"] == uuid.as_str()));
    assert!(list.iter().all(|r| r["code"] != "SYSTEM_ADMIN"));
    assert!(list.iter().all(|r| r.get("permissions").is_none()));
    let res = app
        .get(&format!("/v1/admin/roles/{uuid}/impact"))
        .token(&c.admin.token)
        .send()
        .await;
    ok(&res);
    assert_eq!(res.json, json!({"data": {"count": 0, "users": []}}));
    let (staff_id, staff_uuid) = builtin(&app, "STAFF").await;
    let res = app
        .get(&format!("/v1/admin/roles/{staff_uuid}/impact"))
        .token(&c.admin.token)
        .send()
        .await;
    ok(&res);
    let holders: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "userAccessLevel" WHERE "accessLevelId" = {staff_id} AND "companyId" = {} AND status = 1"#,
            c.id
        ))
        .await;
    assert_eq!(res.data()["count"], holders);

    // Preview (200, belum disimpan).
    let res = app
        .post("/v1/admin/roles/preview")
        .token(&c.admin.token)
        .json(
            &json!({"baseRole": "STAFF", "roleUuid": uuid, "permissions": [
                {"featureKey": "leaves", "actions": ["READ"]},
                {"featureKey": "roles", "actions": ["READ"]},
                {"featureKey": "users", "actions": ["READ"]}
            ]}),
        )
        .send()
        .await;
    res.assert_status(200);
    ok(&res);
    assert_eq!(
        res.text(),
        r#"{"data":{"menusWeb":["Cuti","Peran & Hak Akses","Karyawan"],"menusMobile":["Cuti","Karyawan"],"scopeSummary":"Pada menu Cuti: hanya melihat datanya sendiri","personScopedNotes":[{"feature":"leaves","visibility":"self-only"}],"warnings":[{"code":"SCOPE_TOO_NARROW","feature":"leaves","message":"Role ini bisa membuka Cuti, tapi karena setingkat Staff ia hanya melihat datanya sendiri."},{"code":"OUT_OF_REACH","feature":"roles","message":"Peran & Hak Akses (READ) tidak terjangkau plafon role setingkat STAFF.","minBaseRole":"ADMIN"}],"affectedUsers":0}}"#
    );

    // PATCH (name/description/baseRole) + penolakan.
    let res = app
        .patch(&format!("/v1/admin/roles/{uuid}"))
        .token(&c.admin.token)
        .json(&json!({"name": format!("{name} v2"), "description": null, "baseRole": "STAFF"}))
        .send()
        .await;
    ok(&res);
    assert_eq!(res.data()["name"], format!("{name} v2").as_str());
    assert_eq!(res.data()["description"], Value::Null);
    assert_eq!(res.data()["baseRole"], "STAFF");
    assert_eq!(res.data()["code"], code.as_str(), "code tidak ikut berubah");
    let res = app
        .patch(&format!("/v1/admin/roles/{uuid}"))
        .token(&c.admin.token)
        .json(&json!({"code": "x"}))
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(res.json["code"], "CODE_IS_SERVER_GENERATED");
    // §9.1: role bawaan tidak bisa diubah lewat jalur ini.
    let res = app
        .patch(&format!("/v1/admin/roles/{staff_uuid}"))
        .token(&c.admin.token)
        .json(&json!({"name": "Hacked"}))
        .send()
        .await;
    res.assert_status(404);
    assert_eq!(
        res.text(),
        format!(r#"{{"code":"ROLE_NOT_FOUND","message":"Role '{staff_uuid}' not found"}}"#)
    );

    // PUT permissions (REPLACE).
    let res = app
        .put(&format!("/v1/admin/roles/{uuid}/permissions"))
        .token(&c.admin.token)
        .json(&json!({"permissions": [{"featureKey": "leaves", "actions": ["DELETE"]}]}))
        .send()
        .await;
    ok(&res);
    assert_eq!(
        res.data()["permissions"],
        json!([{"featureKey": "leaves", "actions": ["DELETE", "READ"]}])
    );
    assert!(
        wait_audit(
            &app,
            &format!(
                r#"SELECT COUNT(*) FROM "auditLog" WHERE "entityType" = 'role' AND "entityId" = {id} AND "subEntity" = 'permission'"#
            )
        )
        .await
            > 0,
        "rolePermission (keanggotaan) harus diaudit"
    );

    // Clone (201).
    let res = app
        .post(&format!("/v1/admin/roles/{uuid}/clone"))
        .token(&c.admin.token)
        .json(&json!({"name": format!("Clone {tag}")}))
        .send()
        .await;
    res.assert_status(201);
    ok(&res);
    assert_eq!(res.data()["baseRole"], "STAFF");
    assert_eq!(
        res.data()["permissions"],
        json!([{"featureKey": "leaves", "actions": ["DELETE", "READ"]}])
    );
    let clone_uuid = res.data()["uuid"].as_str().unwrap().to_owned();
    // Clone role bawaan tanpa base role ditolak.
    let (_, super_uuid) = builtin(&app, "SUPER_ADMIN").await;
    let res = app
        .post(&format!("/v1/admin/roles/{super_uuid}/clone"))
        .token(&c.super_admin.token)
        .json(&json!({"name": "SA copy"}))
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(
        res.text(),
        r#"{"code":"SOURCE_HAS_NO_BASE_ROLE","message":"Role 'Super Admin' has no base role and cannot be cloned into a custom role"}"#
    );

    // DELETE tanpa pemegang (200), lalu 404.
    for u in [&uuid, &clone_uuid, dup["uuid"].as_str().unwrap()] {
        let res = app
            .delete(&format!("/v1/admin/roles/{u}"))
            .token(&c.admin.token)
            .send()
            .await;
        ok(&res);
        assert_eq!(
            res.json,
            json!({"data": {"deleted": true, "reassigned": 0}})
        );
    }
    app.get(&format!("/v1/admin/roles/{uuid}"))
        .token(&c.admin.token)
        .send()
        .await
        .assert_status(404);

    // ParseUUIDPipe dan routing Nest untuk path statis `preview`.
    for res in [
        app.get("/v1/admin/roles/not-a-uuid")
            .token(&c.admin.token)
            .send()
            .await,
        app.get("/v1/admin/roles/preview")
            .token(&c.admin.token)
            .send()
            .await,
        app.delete("/v1/admin/roles/preview?reassignTo=x")
            .token(&c.admin.token)
            .send()
            .await,
        app.patch("/v1/admin/roles/123/impact")
            .token(&c.admin.token)
            .send()
            .await,
    ] {
        if res.status.as_u16() == 404 {
            assert_eq!(
                res.json["message"],
                "Cannot PATCH /v1/admin/roles/123/impact"
            );
            continue;
        }
        res.assert_status(400);
        assert_eq!(res.text(), UUID_PIPE);
    }
    // uuid valid tapi body tidak valid: ValidationPipe.
    let res = app
        .put(&format!("/v1/admin/roles/{uuid}/permissions"))
        .token(&c.admin.token)
        .json(&json!({}))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(res.json["message"], json!(["permissions must be an array"]));
    app.put("/v1/admin/roles/preview")
        .token(&c.admin.token)
        .send()
        .await
        .assert_status(404);
}

#[tokio::test]
async fn delete_in_use_requires_reassign() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let original = ual_of(&app, &c, c.staff2.id).await;
    let role = create_role(
        &app,
        &c,
        json!({"name": "Dipakai", "baseRole": "STAFF", "permissions": [{"featureKey": "clock", "actions": ["READ"]}]}),
    )
    .await;
    let uuid = role["uuid"].as_str().unwrap().to_owned();
    let id = role_id(&app, &uuid).await;
    set_ual(&app, &c, c.staff2.id, id).await;

    let res = app
        .delete(&format!("/v1/admin/roles/{uuid}"))
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(
        res.text(),
        r#"{"code":"ROLE_IN_USE","affectedUsers":1,"message":"Role 'Dipakai' is still assigned to 1 user(s). Pass 'reassignTo' with the uuid of the role they should get instead."}"#
    );
    let res = app
        .delete(&format!("/v1/admin/roles/{uuid}?reassignTo={uuid}"))
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(res.json["code"], "REASSIGN_TO_SAME_ROLE");
    // §9.15: ADMIN tidak bisa memindahkan pemegang ke SUPER_ADMIN.
    let (_, super_uuid) = builtin(&app, "SUPER_ADMIN").await;
    let res = app
        .delete(&format!("/v1/admin/roles/{uuid}?reassignTo={super_uuid}"))
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(
        res.json,
        json!({"message": format!("Access Level '{super_uuid}' is above your own access level"),
               "error": "Unprocessable Entity", "statusCode": 422})
    );
    let res = app
        .delete(&format!("/v1/admin/roles/{uuid}?reassignTo=nope"))
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(res.json["message"], json!(["reassignTo must be a UUID"]));

    let (staff_id, staff_uuid) = builtin(&app, "STAFF").await;
    let res = app
        .delete(&format!("/v1/admin/roles/{uuid}?reassignTo={staff_uuid}"))
        .token(&c.admin.token)
        .send()
        .await;
    ok(&res);
    assert_eq!(
        res.json,
        json!({"data": {"deleted": true, "reassigned": 1}})
    );
    assert_eq!(ual_of(&app, &c, c.staff2.id).await, staff_id);
    set_ual(&app, &c, c.staff2.id, original).await;
}

#[tokio::test]
async fn customize_built_in_and_revert() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let (staff_id, staff_uuid) = builtin(&app, "STAFF").await;
    let holders_sql = |level: i32| {
        format!(
            r#"SELECT COUNT(*) FROM "userAccessLevel" WHERE "accessLevelId" = {level} AND "companyId" = {} AND status = 1"#,
            c.id
        )
    };
    let before: i64 = app.scalar(&holders_sql(staff_id)).await;
    assert!(before > 0);

    // Simpan pertama: override dibuat, pemegang company ini pindah.
    let res = app
        .put(&format!("/v1/admin/roles/{staff_uuid}/customize"))
        .token(&c.admin.token)
        .json(&json!({"permissions": [
            {"featureKey": "clock", "actions": ["WRITE"]},
            {"featureKey": "leaves", "actions": ["READ"]}
        ]}))
        .send()
        .await;
    ok(&res);
    let d = res.data().clone();
    let override_uuid = d["uuid"].as_str().unwrap().to_owned();
    assert_ne!(override_uuid, staff_uuid);
    assert_eq!(d["name"], "Staff");
    assert_eq!(d["baseRole"], "STAFF");
    assert_eq!(d["isBuiltInOverride"], true);
    assert_eq!(d["builtInSlot"], "STAFF");
    assert_eq!(d["code"], format!("c{}_staff", c.id).as_str());
    let override_id = role_id(&app, &override_uuid).await;
    assert_eq!(app.scalar::<i64>(&holders_sql(staff_id)).await, 0);
    assert_eq!(app.scalar::<i64>(&holders_sql(override_id)).await, before);
    // Daftar menampilkan override, bukan baris bawaan.
    let res = app
        .get("/v1/admin/roles")
        .token(&c.admin.token)
        .send()
        .await;
    let list = res.data().as_array().unwrap();
    assert!(list.iter().any(|r| r["uuid"] == override_uuid.as_str()));
    assert!(list.iter().all(|r| r["uuid"] != staff_uuid.as_str()));
    // Pemegang override langsung melihat matriks baru (cache diinvalidasi).
    let res = app
        .get("/v1/me/permissions")
        .token(&c.staff.token)
        .send()
        .await;
    let users = res.data()["features"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["key"] == "clock")
        .unwrap()
        .clone();
    assert_eq!(users["actions"], json!(["READ", "WRITE"]));
    assert_eq!(
        res.data()["features"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == "users")
            .unwrap()["actions"],
        json!([])
    );

    // Simpan kedua lewat uuid bawaan ATAU override: override yang sama diperbarui.
    for target in [&staff_uuid, &override_uuid] {
        let res = app
            .put(&format!("/v1/admin/roles/{target}/customize"))
            .token(&c.admin.token)
            .json(&json!({"permissions": [{"featureKey": "clock", "actions": ["READ"]}]}))
            .send()
            .await;
        ok(&res);
        assert_eq!(res.data()["uuid"], override_uuid.as_str());
    }
    // Override: baseRole terkunci, tidak bisa dihapus (pakai revert).
    let res = app
        .patch(&format!("/v1/admin/roles/{override_uuid}"))
        .token(&c.admin.token)
        .json(&json!({"baseRole": "STAFF"}))
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(res.json["code"], "OVERRIDE_BASE_ROLE_FIXED");
    let res = app
        .delete(&format!("/v1/admin/roles/{override_uuid}"))
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(res.json["code"], "OVERRIDE_USE_REVERT");
    // Bawaan tanpa base role & role custom biasa ditolak.
    let (_, super_uuid) = builtin(&app, "SUPER_ADMIN").await;
    let res = app
        .put(&format!("/v1/admin/roles/{super_uuid}/customize"))
        .token(&c.admin.token)
        .json(&json!({"permissions": []}))
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(res.json["code"], "SOURCE_HAS_NO_BASE_ROLE");
    // Revert bawaan (bukan override) = 404.
    app.post(&format!("/v1/admin/roles/{staff_uuid}/revert"))
        .token(&c.admin.token)
        .send()
        .await
        .assert_status(404);

    // Revert (200): pemegang kembali, override soft-delete.
    let res = app
        .post(&format!("/v1/admin/roles/{override_uuid}/revert"))
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(200);
    ok(&res);
    assert_eq!(
        res.json,
        json!({"data": {"reverted": true, "reassigned": before}})
    );
    assert_eq!(app.scalar::<i64>(&holders_sql(staff_id)).await, before);
    let status: i32 = app
        .scalar(&format!(
            r#"SELECT status FROM "accessLevel" WHERE id = {override_id}"#
        ))
        .await;
    assert_eq!(status, 0);

    // Customize lagi sesudah revert: override baru (dulu selalu 409 ROLE_SAVE_CONFLICT karena
    // `accessLevel_company_override_key` ikut menghitung override yang sudah di-soft-delete;
    // migration 20261117100000_access_level_override_active_only).
    let res = app
        .put(&format!("/v1/admin/roles/{staff_uuid}/customize"))
        .token(&c.admin.token)
        .json(&json!({"permissions": [{"featureKey": "clock", "actions": ["READ"]}]}))
        .send()
        .await;
    res.assert_no_sql_error();
    ok(&res);
    let again = res.data()["uuid"].as_str().unwrap().to_owned();
    assert_ne!(again, override_uuid);
    assert_eq!(res.data()["isBuiltInOverride"], true);
    let again_id = role_id(&app, &again).await;
    assert_eq!(app.scalar::<i64>(&holders_sql(staff_id)).await, 0);
    assert_eq!(app.scalar::<i64>(&holders_sql(again_id)).await, before);
    // Dan bisa di-revert lagi.
    app.post(&format!("/v1/admin/roles/{again}/revert"))
        .token(&c.admin.token)
        .send()
        .await
        .assert_status(200);
    assert_eq!(app.scalar::<i64>(&holders_sql(staff_id)).await, before);
}

#[tokio::test]
async fn escalation_and_custom_role_ceiling() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let original = ual_of(&app, &c, c.staff2.id).await;

    // Pemegang role custom setingkat ADMIN dengan matriks terbatas.
    let limited = create_role(
        &app,
        &c,
        json!({"name": "Limited Admin", "baseRole": "ADMIN", "permissions": [rwd("users"), rwd("roles")]}),
    )
    .await;
    let limited_uuid = limited["uuid"].as_str().unwrap().to_owned();
    let limited_id = role_id(&app, &limited_uuid).await;
    set_ual(&app, &c, c.staff2.id, limited_id).await;
    let t = c.staff2.token.clone();

    ok(&app.get("/v1/admin/roles").token(&t).send().await);
    // §9.3: tidak bisa memberi yang tidak dimiliki.
    let res = app
        .post("/v1/admin/roles")
        .token(&t)
        .json(&json!({"name": "Esc", "baseRole": "STAFF", "permissions": [
            {"featureKey": "stores", "actions": ["WRITE"]}, rwd("users")]}))
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(
        res.text(),
        r#"{"code":"PERMISSION_ESCALATION","missing":["stores:WRITE","stores:READ"],"message":"You cannot grant access you do not have yourself: Toko. If you believe you should have it, ask someone with a higher role — or check that the permission backfill has been run in this environment."}"#
    );
    // Clone role bawaan ADMIN = jalan memutar §9.3 — tetap ditolak.
    let (_, admin_uuid) = builtin(&app, "ADMIN").await;
    let res = app
        .post(&format!("/v1/admin/roles/{admin_uuid}/clone"))
        .token(&t)
        .json(&json!({"name": "Admin copy"}))
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(res.json["code"], "PERMISSION_ESCALATION");
    // Menaikkan matriks role sendiri juga ditolak.
    let res = app
        .put(&format!("/v1/admin/roles/{limited_uuid}/permissions"))
        .token(&t)
        .json(&json!({"permissions": [rwd("users"), rwd("roles"), rwd("reports")]}))
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(res.json["missing"][0], "reports:READ");
    let res = app
        .put(&format!("/v1/admin/roles/{admin_uuid}/customize"))
        .token(&t)
        .json(&json!({"permissions": [rwd("users"), rwd("reports")]}))
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(res.json["code"], "PERMISSION_ESCALATION");
    // `users:READ` dimiliki → daftar assignable terbuka untuk role custom setingkat ADMIN.
    let res = app.get("/v1/roles/assignable").token(&t).send().await;
    ok(&res);

    // Plafon role custom (§6.2): base MANAGER tidak menembus @Roles(.., ADMIN).
    let res = app
        .patch(&format!("/v1/admin/roles/{limited_uuid}"))
        .token(&c.admin.token)
        .json(&json!({"baseRole": "MANAGER"}))
        .send()
        .await;
    ok(&res);
    let res = app.get("/v1/admin/roles").token(&t).send().await;
    res.assert_status(403);
    assert_eq!(
        res.json,
        json!({"code":"ROLE_LEVEL_TOO_LOW","message":"Role level is below the minimum required for this endpoint"})
    );
    // Dinaikkan lagi → langsung berlaku (cache RolesGuard ikut diinvalidasi).
    let res = app
        .patch(&format!("/v1/admin/roles/{limited_uuid}"))
        .token(&c.admin.token)
        .json(&json!({"baseRole": "ADMIN"}))
        .send()
        .await;
    ok(&res);
    ok(&app.get("/v1/admin/roles").token(&t).send().await);

    set_ual(&app, &c, c.staff2.id, original).await;
    ok(&app
        .delete(&format!("/v1/admin/roles/{limited_uuid}"))
        .token(&c.admin.token)
        .send()
        .await);
}

#[tokio::test]
async fn permission_change_invalidates_rust_cache() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    let original = ual_of(&app, &c, c.staff2.id).await;
    let role = create_role(
        &app,
        &c,
        json!({"name": "Cache Probe", "baseRole": "ADMIN", "permissions": [rwd("users"), rwd("roles")]}),
    )
    .await;
    let uuid = role["uuid"].as_str().unwrap().to_owned();
    let id = role_id(&app, &uuid).await;
    set_ual(&app, &c, c.staff2.id, id).await;

    // Hangatkan cache permission + RolesGuard pemegang.
    ok(&app
        .get("/v1/admin/roles")
        .token(&c.staff2.token)
        .send()
        .await);
    let key = (c.staff2.id, c.id);
    assert!(app.state.caches.permissions.get_fresh(&key).is_some());
    assert!(app.state.caches.roles.get_fresh(&key).is_some());

    // Cabut `roles` (tinggal `users`): harus terasa SEKARANG, bukan sesudah TTL 60 dtk.
    let res = app
        .put(&format!("/v1/admin/roles/{uuid}/permissions"))
        .token(&c.admin.token)
        .json(&json!({"permissions": [rwd("users")]}))
        .send()
        .await;
    ok(&res);
    assert!(app.state.caches.permissions.get_fresh(&key).is_none());
    assert!(app.state.caches.roles.get_fresh(&key).is_none());
    let res = app
        .get("/v1/admin/roles")
        .token(&c.staff2.token)
        .send()
        .await;
    res.assert_status(403);
    assert_eq!(
        res.json,
        json!({"code":"PERMISSION_DENIED","feature":"roles","action":"READ",
               "message":"Role ini tidak punya izin READ untuk fitur roles"})
    );
    // `GET /v1/me/permissions` ikut melihat matriks baru.
    let res = app
        .get("/v1/me/permissions")
        .token(&c.staff2.token)
        .send()
        .await;
    ok(&res);
    let roles_row = res.data()["features"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["key"] == "roles")
        .unwrap()
        .clone();
    assert_eq!(roles_row["actions"], json!([]));
    assert_eq!(res.data()["dataScope"], "COMPANY");

    set_ual(&app, &c, c.staff2.id, original).await;
    ok(&app
        .delete(&format!("/v1/admin/roles/{uuid}"))
        .token(&c.admin.token)
        .send()
        .await);
}

#[tokio::test]
async fn anti_lockout_rejects_last_admin_removal() {
    let _g = SERIAL.lock().await;
    let Some((app, c)) = setup().await else {
        return;
    };
    // Sisakan SATU admin aktif (c.admin): admin lain dinonaktifkan sementara.
    let others: Vec<i32> = sqlx::query_scalar(
        r#"SELECT u.id FROM users u
           JOIN "userAccessLevel" ual ON ual."userId" = u.id AND ual."companyId" = $1 AND ual.status = 1
           JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE u.status = 1 AND u.id <> $2 AND al.code IN ('SYSTEM_ADMIN', 'SUPER_ADMIN', 'ADMIN')"#,
    )
    .bind(c.id)
    .bind(c.admin.id)
    .fetch_all(app.db())
    .await
    .unwrap();
    assert!(!others.is_empty());
    sqlx::query("UPDATE users SET status = 3 WHERE id = ANY($1)")
        .bind(&others)
        .execute(app.db())
        .await
        .unwrap();
    let admin_name: String = app
        .scalar(&format!("SELECT name FROM users WHERE id = {}", c.admin.id))
        .await;
    let (admin_id, admin_uuid) = builtin(&app, "ADMIN").await;
    let overrides_before: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "accessLevel" WHERE "companyId" = {} AND "overridesRoleId" = {admin_id}"#,
            c.id
        ))
        .await;

    // Mencabut `users:WRITE` dari ADMIN (lewat override) = company tanpa pengelola.
    let res = app
        .put(&format!("/v1/admin/roles/{admin_uuid}/customize"))
        .token(&c.admin.token)
        .json(&json!({"permissions": [rwd("roles"), {"featureKey": "users", "actions": ["READ"]}]}))
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(
        res.text(),
        format!(
            r#"{{"code":"LAST_COMPANY_ADMIN","lastAdmins":["{admin_name}"],"message":"This change would leave the company with nobody who can manage roles and users. Grant another active user 'roles' and 'users' write access first (currently only: {admin_name})."}}"#
        )
    );
    // Rollback: tidak ada override, pemegang tetap di ADMIN, tidak ada baris audit.
    let overrides_after: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "accessLevel" WHERE "companyId" = {} AND "overridesRoleId" = {admin_id}"#,
            c.id
        ))
        .await;
    assert_eq!(overrides_after, overrides_before);
    assert_eq!(ual_of(&app, &c, c.admin.id).await, admin_id);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let audit_rows: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "auditLog" WHERE route = 'PUT /v1/admin/roles/{admin_uuid}/customize'"#
        ))
        .await;
    assert_eq!(
        audit_rows, 0,
        "transaksi batal = audit dibuang (discardAudit)"
    );

    // Jalur §9.6 lain: role custom admin-satu-satunya kehilangan `roles:WRITE`.
    let role = create_role(
        &app,
        &c,
        json!({"name": "Sole Admin", "baseRole": "ADMIN", "permissions": [rwd("users"), rwd("roles")]}),
    )
    .await;
    let uuid = role["uuid"].as_str().unwrap().to_owned();
    let id = role_id(&app, &uuid).await;
    set_ual(&app, &c, c.admin.id, id).await;
    let res = app
        .put(&format!("/v1/admin/roles/{uuid}/permissions"))
        .token(&c.admin.token)
        .json(&json!({"permissions": [rwd("users"), {"featureKey": "roles", "actions": ["READ"]}]}))
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(res.json["code"], "LAST_COMPANY_ADMIN");
    // Menghapus role itu (pindah ke STAFF) juga mengunci company.
    let (_, staff_uuid) = builtin(&app, "STAFF").await;
    let res = app
        .delete(&format!("/v1/admin/roles/{uuid}?reassignTo={staff_uuid}"))
        .token(&c.admin.token)
        .send()
        .await;
    res.assert_status(422);
    assert_eq!(res.json["code"], "LAST_COMPANY_ADMIN");

    // Pulihkan.
    set_ual(&app, &c, c.admin.id, admin_id).await;
    sqlx::query("UPDATE users SET status = 1 WHERE id = ANY($1)")
        .bind(&others)
        .execute(app.db())
        .await
        .unwrap();
    ok(&app
        .delete(&format!("/v1/admin/roles/{uuid}"))
        .token(&c.admin.token)
        .send()
        .await);
}
