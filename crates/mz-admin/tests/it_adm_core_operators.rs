//! Uji integrasi `admin.controller.ts`: login/lupa password operator, daftar/detail/CRUD operator,
//! analytics. Jalankan:
//! `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-admin --test it_adm_core_operators`.

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
use tokio::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::const_new(());

fn build(state: &AppState) -> Router {
    mz_admin::router(state)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

fn req<'a>(app: &'a TestApp, m: &str, path: &str) -> mz_testkit::RequestBuilder<'a> {
    match m {
        "GET" => app.get(path),
        "POST" => app.post(path),
        "PATCH" => app.patch(path),
        _ => app.delete(path),
    }
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

struct Op {
    id: i32,
    uuid: String,
    username: String,
    token: String,
}

async fn operator_with(app: &TestApp, role: &str, status: i32, password: &str) -> Op {
    let u = uniq();
    let username = format!("op{u}@it.test");
    let hash = bcrypt::hash(password, 4).unwrap();
    let (id, uuid): (i32, String) = sqlx::query_as(
        r#"INSERT INTO operators (username, password, name, status, role)
           VALUES ($1, $2, 'IT Operator', $3, $4::"OperatorRole") RETURNING id, uuid::text"#,
    )
    .bind(&username)
    .bind(hash)
    .bind(status)
    .bind(role)
    .fetch_one(app.db())
    .await
    .unwrap();
    Op {
        id,
        uuid,
        username,
        token: app.token_with(json!({ "username": "x", "sub": id })),
    }
}

async fn operator(app: &TestApp, role: &str) -> Op {
    operator_with(app, role, 1, "initial-pass").await
}

/// (action, targetType, targetUuid, before, after, reason).
type AuditRow = (
    String,
    String,
    Option<String>,
    Option<Value>,
    Option<Value>,
    Option<String>,
);

async fn audit_rows(app: &TestApp, op: i32) -> Vec<AuditRow> {
    sqlx::query_as(
        r#"SELECT action, "targetType", "targetUuid"::text, before, after, reason
             FROM "operatorAuditLog" WHERE "operatorId" = $1 ORDER BY id"#,
    )
    .bind(op)
    .fetch_all(app.db())
    .await
    .unwrap()
}

#[tokio::test]
async fn gate_list_detail_and_validation() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;
    let owner = operator(&app, "OWNER").await;
    let support = operator(&app, "SUPPORT").await;
    let inactive = operator_with(&app, "OWNER", 0, "initial-pass").await;

    // 401 polos: tanpa token, token user biasa, operator nonaktif — untuk semua route operator.
    for (m, path) in [
        ("GET", "/admin"),
        ("GET", "/admin/analytics/users"),
        ("GET", "/admin/analytics/live-activity"),
        ("GET", "/admin/create"),
        ("POST", "/admin/create"),
        (
            "PATCH",
            "/admin/update/00000000-0000-4000-8000-000000000000",
        ),
        (
            "DELETE",
            "/admin/delete/00000000-0000-4000-8000-000000000000",
        ),
    ] {
        let r = req(&app, m, path).send().await;
        r.assert_status(401);
        assert_eq!(
            r.json,
            json!({"message": "Unauthorized", "statusCode": 401}),
            "{m} {path}"
        );
        req(&app, m, path)
            .user(c.admin.id)
            .send()
            .await
            .assert_status(401);
        req(&app, m, path)
            .token(&inactive.token)
            .send()
            .await
            .assert_status(401);
    }

    // Peran: hanya OWNER yang boleh create/update/delete (guard SEBELUM validasi body).
    let r = app
        .post("/admin/create")
        .token(&support.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(403);
    assert_eq!(
        r.json,
        json!({"code":"OPERATOR_ROLE_FORBIDDEN","message":"Operator role is not allowed for this endpoint"})
    );

    // Daftar: semua peran boleh (hanya JwtAdminAuthGuard); bentuk respons.
    let r = app.get("/admin").token(&support.token).send().await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["success"], json!(true));
    let data = &r.json["data"];
    assert_eq!(data["pageNumber"], json!(1));
    assert_eq!(data["itemsPerPage"], json!(10));
    assert!(data["total"].as_i64().unwrap() >= 3);
    let first = &data["results"][0];
    let keys: Vec<&str> = first
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["id", "uuid", "username", "name", "role", "status"]);
    assert_eq!(
        first["id"].as_i64(),
        data["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_i64().unwrap())
            .max(),
        "urut id menurun"
    );

    // Pencarian (case-insensitive) + batas itemsPerPage 100 + paginasi.
    let r = app
        .get(&format!(
            "/admin?search={}&itemsPerPage=1000&pageNumber=1",
            owner.username.to_uppercase().replace('@', "%40")
        ))
        .token(&owner.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["total"], json!(1));
    assert_eq!(r.json["data"]["itemsPerPage"], json!(100));
    assert_eq!(r.json["data"]["results"][0]["uuid"], json!(owner.uuid));
    // Halaman di luar jangkauan → kosong; pageNumber negatif → 1.
    let r = app
        .get("/admin?pageNumber=-4&itemsPerPage=0")
        .token(&owner.token)
        .send()
        .await;
    assert_eq!(r.json["data"]["pageNumber"], json!(1));
    assert_eq!(r.json["data"]["itemsPerPage"], json!(10));
    // Validasi pipe global: angka tak valid / array.
    let r = app
        .get("/admin?pageNumber=abc&search[]=a")
        .token(&owner.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json,
        json!({"message":["pageNumber must be a number conforming to the specified constraints","search must be a string"],"error":"Bad Request","statusCode":400})
    );

    // Detail: ada / tak ada (data `{}`) / bukan uuid (galat ditangkap Node → success false).
    let r = app
        .get(&format!("/admin/{}", owner.uuid))
        .token(&support.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["username"], json!(owner.username));
    assert!(r.json["data"].get("password").is_none());
    let r = app
        .get("/admin/00000000-0000-4000-8000-000000000000")
        .token(&support.token)
        .send()
        .await;
    assert_eq!(r.json, json!({"success": true, "data": {}}));
    for odd in ["not-a-uuid", "create"] {
        let r = app
            .get(&format!("/admin/{odd}"))
            .token(&support.token)
            .send()
            .await;
        r.assert_status(200);
        assert_eq!(
            r.json,
            json!({"success": false, "message": "Error retrieving admin data", "data": null}),
            "{odd}"
        );
    }
}

#[tokio::test]
async fn crud_roles_and_audit() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let owner = operator(&app, "OWNER").await;
    let u = uniq();
    let new_name = format!("new{u}@it.test");

    // Validasi body: pipe global Node lebih dulu → properti asing tidak dilaporkan selama ada
    // galat field lain.
    let r = app
        .post("/admin/create")
        .token(&owner.token)
        .json(&json!({"username":"nope","password":"short","name":"","role":"ROOT","zzz":1}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "username must be an email",
            "password must be longer than or equal to 8 characters",
            "name should not be empty",
            "role must be one of the following values: OWNER, FINANCE, SUPPORT"
        ])
    );
    assert!(audit_rows(&app, owner.id).await.is_empty());

    // Create (201): username di-trim + lowercase, role default SUPPORT, password di-hash, audit tanpa rahasia.
    let r = app
        .post("/admin/create")
        .token(&owner.token)
        .json(&json!({"username": format!("  {}  ", new_name.to_uppercase()), "password": "password-123", "name": " Budi "}))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    assert_eq!(r.json["message"], json!("Admin created successfully"));
    let created = r.json["data"].clone();
    assert_eq!(created["username"], json!(new_name));
    assert_eq!(created["name"], json!("Budi"));
    assert_eq!(created["role"], json!("SUPPORT"));
    assert_eq!(created["status"], json!(1));
    assert!(created.get("password").is_none());
    let new_uuid = created["uuid"].as_str().unwrap().to_owned();
    let new_id = created["id"].as_i64().unwrap() as i32;
    let hash: String = app
        .scalar(&format!(
            "SELECT password FROM operators WHERE id = {new_id}"
        ))
        .await;
    assert!(hash.starts_with("$2") && bcrypt::verify("password-123", &hash).unwrap());
    let rows = audit_rows(&app, owner.id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "operator.create");
    assert_eq!(rows[0].1, "operator");
    assert_eq!(rows[0].2.as_deref(), Some(new_uuid.as_str()));
    assert_eq!(
        rows[0].4,
        Some(json!({"username": new_name, "name": "Budi", "role": "SUPPORT", "status": 1}))
    );

    // Username kembar → 409 USERNAME_TAKEN (create & update).
    let r = app
        .post("/admin/create")
        .token(&owner.token)
        .json(&json!({"username": new_name, "password": "password-123", "name": "X"}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(
        r.json,
        json!({"code":"USERNAME_TAKEN","message":"Username is already in use"})
    );
    let r = app
        .patch(&format!("/admin/update/{new_uuid}"))
        .token(&owner.token)
        .json(&json!({"username": owner.username}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], json!("USERNAME_TAKEN"));

    // Update: uuid tak valid / tak ada.
    let r = app
        .patch("/admin/update/xyz")
        .token(&owner.token)
        .json(&json!({"name": "A"}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json,
        json!({"message":"Validation failed (uuid is expected)","error":"Bad Request","statusCode":400})
    );
    let ghost = "00000000-0000-4000-8000-0000000000aa";
    let r = app
        .patch(&format!("/admin/update/{ghost}"))
        .token(&owner.token)
        .json(&json!({"name": "A"}))
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(
        r.json,
        json!({"code":"OPERATOR_NOT_FOUND","message":format!("Admin with uuid {ghost} not found")})
    );
    // Body asing ditolak (whitelist).
    let r = app
        .patch(&format!("/admin/update/{new_uuid}"))
        .token(&owner.token)
        .json(&json!({"password": "hack-attempt-1"}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!(["property password should not exist"])
    );

    // Update nama saja: tanpa reason, audit before/after hanya kolom berubah.
    let r = app
        .patch(&format!("/admin/update/{new_uuid}"))
        .token(&owner.token)
        .json(&json!({"name": "Budi Baru"}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["message"], json!("Admin data updated successfully"));
    assert_eq!(r.json["data"]["name"], json!("Budi Baru"));
    let rows = audit_rows(&app, owner.id).await;
    let last = rows.last().unwrap();
    assert_eq!(last.0, "operator.update");
    assert_eq!(last.3, Some(json!({"name": "Budi"})));
    assert_eq!(last.4, Some(json!({"name": "Budi Baru"})));
    assert_eq!(last.5, None);
    // Tanpa perubahan → data apa adanya, tanpa baris audit baru.
    let n = audit_rows(&app, owner.id).await.len();
    let r = app
        .patch(&format!("/admin/update/{new_uuid}"))
        .token(&owner.token)
        .json(&json!({"name": "Budi Baru"}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(audit_rows(&app, owner.id).await.len(), n);

    // Role/status berubah wajib reason.
    let r = app
        .patch(&format!("/admin/update/{new_uuid}"))
        .token(&owner.token)
        .json(&json!({"role": "FINANCE"}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json,
        json!({"code":"REASON_REQUIRED","message":"reason is required when role or status changes"})
    );
    let r = app
        .patch(&format!("/admin/update/{new_uuid}"))
        .token(&owner.token)
        .json(&json!({"role": "FINANCE", "reason": "  naik jabatan  "}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["role"], json!("FINANCE"));
    let rows = audit_rows(&app, owner.id).await;
    let last = rows.last().unwrap();
    assert_eq!(last.3, Some(json!({"role": "SUPPORT"})));
    assert_eq!(last.4, Some(json!({"role": "FINANCE"})));
    assert_eq!(last.5.as_deref(), Some("naik jabatan"));

    // Menonaktifkan diri sendiri ditolak.
    let r = app
        .patch(&format!("/admin/update/{}", owner.uuid))
        .token(&owner.token)
        .json(&json!({"status": 0, "reason": "mau berhenti"}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], json!("CANNOT_REMOVE_SELF"));
    let r = app
        .delete(&format!("/admin/delete/{}", owner.uuid))
        .token(&owner.token)
        .json(&json!({"reason": "hapus diri sendiri"}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], json!("CANNOT_REMOVE_SELF"));

    // Delete: reason wajib; soft delete + cabut token MCP; audit `operator.deactivate`.
    let r = app
        .delete(&format!("/admin/delete/{new_uuid}"))
        .token(&owner.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "reason must be longer than or equal to 5 characters",
            "reason must be a string"
        ])
    );
    app.exec(&format!(
        r#"INSERT INTO "mcpAdminAccessToken" ("operatorId", name, "tokenHash", "tokenPrefix")
           VALUES ({new_id}, 'it', 'hash{u}', 'pfx')"#
    ))
    .await;
    let r = app
        .delete(&format!("/admin/delete/{new_uuid}"))
        .token(&owner.token)
        .json(&json!({"reason": "akun tidak dipakai"}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["message"], json!("Admin deactivated successfully"));
    assert_eq!(r.json["data"]["status"], json!(0));
    let revoked: bool = app
        .scalar(&format!(
            r#"SELECT "revokedAt" IS NOT NULL FROM "mcpAdminAccessToken" WHERE "operatorId" = {new_id}"#
        ))
        .await;
    assert!(revoked);
    let rows = audit_rows(&app, owner.id).await;
    let last = rows.last().unwrap();
    assert_eq!(last.0, "operator.deactivate");
    assert_eq!(last.3, Some(json!({"status": 1, "role": "FINANCE"})));
    assert_eq!(last.4, Some(json!({"status": 0, "role": "FINANCE"})));
    assert_eq!(last.5.as_deref(), Some("akun tidak dipakai"));
    // Baris tidak pernah dihapus; operator nonaktif langsung 401.
    let still: i64 = app
        .scalar(&format!(
            "SELECT COUNT(*) FROM operators WHERE id = {new_id}"
        ))
        .await;
    assert_eq!(still, 1);
    let dead = app.token_with(json!({"username": "x", "sub": new_id}));
    app.get("/admin")
        .token(&dead)
        .send()
        .await
        .assert_status(401);
    app.delete(&format!("/admin/delete/{ghost}"))
        .token(&owner.token)
        .json(&json!({"reason": "tidak ada"}))
        .send()
        .await
        .assert_status(404);
}

#[tokio::test]
async fn login_and_forgot_password() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let op = operator_with(&app, "OWNER", 1, "initial-pass").await;
    let gone = operator_with(&app, "OWNER", 0, "initial-pass").await;

    // Validasi (pipe global, tanpa whitelist).
    let r = app.post("/admin/auth/login").json(&json!({})).send().await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "username should not be empty",
            "username must be an email",
            "password must be longer than or equal to 6 characters",
            "password should not be empty",
            "password must be a string"
        ])
    );

    // Password salah / operator tak ada: satu pesan, status 201.
    for body in [
        json!({"username": op.username, "password": "wrong-pass"}),
        json!({"username": format!("nobody{}@it.test", uniq()), "password": "wrong-pass"}),
    ] {
        let r = app.post("/admin/auth/login").json(&body).send().await;
        r.assert_status(201);
        assert_eq!(
            r.json,
            json!({"success": false, "message": "Invalid username or password"})
        );
    }
    // Operator nonaktif: hanya pemilik password yang diberi tahu.
    let r = app
        .post("/admin/auth/login")
        .json(&json!({"username": gone.username, "password": "initial-pass"}))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(
        r.json,
        json!({"success": false, "message": "Admin account is inactive"})
    );

    // Sukses: bentuk respons + token dipakai extractor operator (payload { username, sub }).
    let r = app
        .post("/admin/auth/login")
        .json(&json!({"username": op.username, "password": "initial-pass"}))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(r.json["success"], json!(true));
    assert_eq!(r.json["message"], json!("Admin login successful"));
    let d = &r.json["data"];
    let keys: Vec<&str> = d.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["token", "id", "uuid", "username", "name", "role"]);
    assert_eq!(d["id"], json!(op.id));
    assert_eq!(d["role"], json!("OWNER"));
    let token = d["token"].as_str().unwrap().to_owned();
    let claims =
        mz_operator::decode_operator_claims(&app.state().config.jwt_secret, &token).unwrap();
    assert_eq!(claims["sub"], json!(op.id));
    assert_eq!(claims["username"], json!(op.username));
    assert!(claims.contains_key("iat"));
    app.get("/admin")
        .token(&token)
        .send()
        .await
        .assert_status(200);

    // Kunci per username: 10 percobaan lolos ke bcrypt, ke-11 terkunci (bucket `la:operator:*`).
    let locked_user = format!("lock{}@it.test", uniq());
    for _ in 0..10 {
        let r = app
            .post("/admin/auth/login")
            .json(&json!({"username": locked_user, "password": "wrong-pass"}))
            .send()
            .await;
        assert_eq!(r.json["message"], json!("Invalid username or password"));
    }
    let r = app
        .post("/admin/auth/login")
        .json(&json!({"username": locked_user, "password": "wrong-pass"}))
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({"success": false, "message": "Too many failed login attempts. Please try again in 15 minutes."})
    );

    // ── Lupa password ──
    let generic = json!({"success": true, "message": "If the email is registered, a password reset link has been sent."});
    let r = app
        .post("/admin/auth/forgot-password/generate")
        .json(&json!({"username": format!("ghost{}@it.test", uniq())}))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(r.json, generic);
    let r = app
        .post("/admin/auth/forgot-password/generate")
        .json(&json!({"username": "bukan-email"}))
        .send()
        .await;
    r.assert_status(400);
    let r = app
        .post("/admin/auth/forgot-password/generate")
        .json(&json!({"username": op.username}))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(r.json, generic);
    // Email dikirim di latar belakang (gagal tanpa token Mailtrap); ia menandai token "terkirim" (status 0).
    let mut token_uuid = String::new();
    for _ in 0..50 {
        let row: Option<(String, i32)> = sqlx::query_as(
            r#"SELECT uuid::text, status FROM "operatorForgotPassword" WHERE "operatorId" = $1"#,
        )
        .bind(op.id)
        .fetch_optional(app.db())
        .await
        .unwrap();
        if let Some((u, 0)) = row {
            token_uuid = u;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        !token_uuid.is_empty(),
        "token reset harus berstatus 0 setelah dikirim"
    );
    let (created_at, expired_at): (chrono::NaiveDateTime, chrono::NaiveDateTime) = sqlx::query_as(
        r#"SELECT "createdAt", "expiredAt" FROM "operatorForgotPassword" WHERE uuid = $1::uuid"#,
    )
    .bind(&token_uuid)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!((expired_at - created_at).num_minutes(), 30, "EXPIRED_AT=30");

    // Reset: token/username salah → gagal; benar → sukses sekali pakai.
    let fail = json!({"success": false, "message": "Invalid or expired token"});
    for body in [
        json!({"username": op.username, "password": "brand-new-pass", "forgotPasswordUuid": uuid::Uuid::new_v4().to_string()}),
        json!({"username": format!("ghost{}@it.test", uniq()), "password": "brand-new-pass", "forgotPasswordUuid": token_uuid}),
    ] {
        let r = app
            .post("/admin/auth/forgot-password")
            .json(&body)
            .send()
            .await;
        r.assert_status(201);
        assert_eq!(r.json, fail);
    }
    let r = app
        .post("/admin/auth/forgot-password")
        .json(&json!({"username": op.username, "password": "short", "forgotPasswordUuid": "x"}))
        .send()
        .await;
    r.assert_status(400);
    let body = json!({"username": op.username, "password": "brand-new-pass", "forgotPasswordUuid": token_uuid});
    let r = app
        .post("/admin/auth/forgot-password")
        .json(&body)
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(r.json["success"], json!(true));
    assert_eq!(r.json["message"], json!("Reset password success"));
    let keys: Vec<&str> = r.json["data"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "id",
            "username",
            "name",
            "createdAt",
            "updatedAt",
            "status",
            "uuid"
        ]
    );
    // Pakai ulang ditolak; password baru berlaku, yang lama tidak.
    let r = app
        .post("/admin/auth/forgot-password")
        .json(&body)
        .send()
        .await;
    assert_eq!(r.json, fail);
    let r = app
        .post("/admin/auth/login")
        .json(&json!({"username": op.username, "password": "initial-pass"}))
        .send()
        .await;
    assert_eq!(r.json["success"], json!(false));
    let r = app
        .post("/admin/auth/login")
        .json(&json!({"username": op.username, "password": "brand-new-pass"}))
        .send()
        .await;
    assert_eq!(r.json["success"], json!(true));
    let leftover: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "operatorForgotPassword" WHERE "operatorId" = {} AND status IN (0, 1)"#,
            op.id
        ))
        .await;
    assert_eq!(leftover, 0);
}

#[tokio::test]
async fn analytics() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;
    let op = operator(&app, "FINANCE").await;

    // live-activity: bentuk + cache Redis 60 dtk (panggilan kedua mengembalikan `generatedAt` sama).
    let r = app
        .get("/admin/analytics/live-activity")
        .token(&op.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["success"], json!(true));
    assert_eq!(r.json["message"], json!("Success get live activity"));
    let d = r.json["data"].clone();
    let keys: Vec<&str> = d.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "date",
            "totalUserTracking",
            "totalSalesOnVisit",
            "generatedAt"
        ]
    );
    assert!(d["totalUserTracking"].is_i64() && d["totalSalesOnVisit"].is_i64());
    let r2 = app
        .get("/admin/analytics/live-activity")
        .token(&op.token)
        .send()
        .await;
    assert_eq!(r2.json["data"], d);

    // users: owner (SUPER_ADMIN) company fixture yang mendaftar baru + karyawan dengan timeline.
    app.exec(&format!(
        r#"UPDATE users SET "createdAt" = NOW() WHERE id = {}"#,
        c.super_admin.id
    ))
    .await;
    let r = app
        .get("/admin/analytics/users?days=3&itemsPerPage=1000&pageNumber=1")
        .token(&op.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(r.json["success"], json!(true));
    assert_eq!(r.json["message"], json!("Success get user analytics"));
    let data = &r.json["data"];
    assert_eq!(data["pageNumber"], json!(1));
    assert_eq!(data["itemsPerPage"], json!(1000));
    let result = data["result"].as_array().unwrap();
    assert_eq!(data["total"].as_i64().unwrap() as usize, result.len());
    let mine = result
        .iter()
        .find(|o| o["id"] == json!(c.super_admin.id))
        .expect("owner fixture terdaftar");
    let keys: Vec<&str> = mine
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "id",
            "uuid",
            "username",
            "name",
            "createdAt",
            "hasDownloadedApp",
            "company",
            "tracking",
            "employees"
        ]
    );
    assert_eq!(mine["company"]["uuid"], json!(c.uuid));
    assert_eq!(
        mine["tracking"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["hasTracked", "totalTracking", "lastTrackingAt"]
    );
    let emp = mine["employees"].as_array().unwrap();
    assert!(!emp.is_empty());
    assert_eq!(
        emp[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "id",
            "uuid",
            "username",
            "name",
            "createdAt",
            "hasDownloadedApp",
            "hasTrack",
            "totalTracking",
            "lastTrackingAt"
        ]
    );
    // Ringkasan tracking cocok dengan jumlah segmen aktif karyawan.
    for e in emp {
        let uid = e["id"].as_i64().unwrap();
        let (n, total): (i64, i64) = sqlx::query_as(
            r#"SELECT COUNT(*), COALESCE(SUM(FLOOR(EXTRACT(EPOCH FROM (tt."endAt" - tt."startAt")))), 0)::bigint
                 FROM tracking t JOIN "trackingTimeline" tt ON tt."trackingId" = t.id AND tt.status = 1
                WHERE t."userId" = $1 AND t.status = 1"#,
        )
        .bind(uid as i32)
        .fetch_one(app.db())
        .await
        .unwrap();
        assert_eq!(e["hasTrack"], json!(n > 0), "user {uid}");
        assert_eq!(e["totalTracking"], json!(total), "user {uid}");
        assert_eq!(e["lastTrackingAt"].is_string(), n > 0, "user {uid}");
    }

    // Query sampah → bawaan (days 7, page 1, 20); halaman negatif → success false (ditangkap Node).
    let r = app
        .get("/admin/analytics/users?days=abc&pageNumber=x&itemsPerPage=")
        .token(&op.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["itemsPerPage"], json!(20));
    let r = app
        .get("/admin/analytics/users?pageNumber=-3")
        .token(&op.token)
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({"success": false, "message": "Error retrieving user analytics", "data": null})
    );
}
