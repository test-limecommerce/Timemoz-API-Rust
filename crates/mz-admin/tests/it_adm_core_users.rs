//! Uji integrasi `admin/users/**` (8 route). Jalankan:
//! `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-admin --test it_adm_core_users`.

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
        _ => app.patch(path),
    }
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

struct Op {
    token: String,
}

async fn operator(app: &TestApp, role: &str) -> Op {
    let u = uniq();
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO operators (username, password, name, status, role)
               VALUES ('op{u}@it.test', 'x', 'IT Operator', 1, '{role}'::"OperatorRole") RETURNING id"#
        ))
        .await;
    Op {
        token: app.token_with(json!({ "username": "x", "sub": id })),
    }
}

struct U {
    id: i32,
    uuid: String,
    email: String,
}

async fn user(app: &TestApp, status: i32) -> U {
    let u = uniq();
    let email = format!("usr{u}@it.test");
    let (id, uuid): (i32, String) = sqlx::query_as(
        r#"INSERT INTO users (username, password, name, status) VALUES ($1, 'x', $2, $3)
           RETURNING id, uuid::text"#,
    )
    .bind(&email)
    .bind(format!("IT User {u}"))
    .bind(status)
    .fetch_one(app.db())
    .await
    .unwrap();
    U { id, uuid, email }
}

/// Anggota company `company` (userCompany status 1) dengan role bawaan `role`.
async fn member(app: &TestApp, u: &U, company: i32, role: &str) {
    app.exec(&format!(
        r#"INSERT INTO "userCompany" ("userId", "companyId", status, "idleTimeInSecond", "isDefault")
           VALUES ({uid}, {company}, 1, 180, true);
           INSERT INTO "userAccessLevel" ("userId", "accessLevelId", "companyId", status)
           VALUES ({uid}, (SELECT id FROM "accessLevel" WHERE code = '{role}' AND "isSystem" ORDER BY id LIMIT 1), {company}, 1);"#,
        uid = u.id
    ))
    .await;
}

async fn new_company(app: &TestApp, owner: Option<i32>) -> i32 {
    let u = uniq();
    let owner = owner.map_or("NULL".to_owned(), |o| o.to_string());
    app.scalar(&format!(
        r#"INSERT INTO company (name, status, "ownerUserId", register_email)
           VALUES ('it-adm-users-{u}', 1, {owner}, 'old@it.test') RETURNING id"#
    ))
    .await
}

type OpAudit = (
    String,
    Option<String>,
    Option<Value>,
    Option<Value>,
    Option<String>,
);

async fn op_audit(app: &TestApp, uuid: &str) -> Vec<OpAudit> {
    sqlx::query_as(
        r#"SELECT action, "targetType", before, after, reason
             FROM "operatorAuditLog" WHERE "targetUuid" = $1::uuid ORDER BY id"#,
    )
    .bind(uuid)
    .fetch_all(app.db())
    .await
    .unwrap()
    .into_iter()
    .map(
        |(a, t, b, af, r): (String, String, Option<Value>, Option<Value>, Option<String>)| {
            assert_eq!(t, "user");
            (a, Some(t), b, af, r)
        },
    )
    .collect()
}

async fn user_status(app: &TestApp, id: i32) -> i32 {
    app.scalar(&format!("SELECT status FROM users WHERE id = {id}"))
        .await
}

#[tokio::test]
async fn gate_roles_validation_search_detail() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;
    let owner = operator(&app, "OWNER").await;
    let finance = operator(&app, "FINANCE").await;
    let support = operator(&app, "SUPPORT").await;
    let u = user(&app, 1).await;
    let company = new_company(&app, Some(u.id)).await;
    member(&app, &u, company, "ADMIN").await;
    let other = user(&app, 3).await;
    let ghost = "00000000-0000-4000-8000-0000000000ab";

    // 401 polos untuk semua 8 route.
    let routes: [(&str, String); 8] = [
        ("GET", "/admin/users".to_owned()),
        ("GET", format!("/admin/users/{}", u.uuid)),
        (
            "POST",
            format!("/admin/users/{}/send-reset-password", u.uuid),
        ),
        ("POST", format!("/admin/users/{}/verify-email", u.uuid)),
        ("PATCH", format!("/admin/users/{}/email", u.uuid)),
        ("POST", format!("/admin/users/{}/disable", u.uuid)),
        ("POST", format!("/admin/users/{}/enable", u.uuid)),
        ("POST", format!("/admin/users/{}/revoke-sessions", u.uuid)),
    ];
    for (m, path) in &routes {
        let r = req(&app, m, path).send().await;
        r.assert_status(401);
        assert_eq!(
            r.json,
            json!({"message": "Unauthorized", "statusCode": 401})
        );
        req(&app, m, path)
            .user(c.admin.id)
            .send()
            .await
            .assert_status(401);
    }

    // FINANCE: baca boleh, tulis 403 (guard sebelum validasi body/uuid).
    for (m, path) in routes.iter().skip(2) {
        let r = match *m {
            "POST" => app.post(path),
            _ => app.patch(path),
        }
        .token(&finance.token)
        .json(&json!({}))
        .send()
        .await;
        r.assert_status(403);
        assert_eq!(r.json["code"], json!("OPERATOR_ROLE_FORBIDDEN"), "{path}");
    }
    app.get("/admin/users")
        .token(&finance.token)
        .send()
        .await
        .assert_status(200);

    // Validasi query (urutan pesan) + galat body lebih dulu dari uuid pipe.
    let r = app
        .get("/admin/users?foo=1&search=ab&status=X&pageNumber=0&itemsPerPage=99")
        .token(&support.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "search must be longer than or equal to 3 characters",
            "status must be one of the following values: ACTIVE, INVITED, DISABLED, DELETED",
            "pageNumber must not be less than 1",
            "itemsPerPage must not be greater than 50"
        ])
    );
    let r = app
        .get("/admin/users/not-a-uuid")
        .token(&support.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!("Validation failed (uuid is expected)")
    );
    // Galat body (pipe global) lebih dulu dari `ParseUUIDPipe`; body sah → galat uuid.
    let r = app
        .post("/admin/users/not-a-uuid/disable")
        .token(&support.token)
        .json(&json!({}))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!([
            "reason must be longer than or equal to 5 characters",
            "reason must be a string"
        ])
    );
    let r = app
        .post("/admin/users/not-a-uuid/disable")
        .token(&support.token)
        .json(&json!({"reason": "alasan sah"}))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!("Validation failed (uuid is expected)")
    );
    let r = app
        .post(&format!("/admin/users/{}/disable", u.uuid))
        .token(&support.token)
        .json(&json!({"reason": "ok", "x": 1}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!(["reason must be longer than or equal to 5 characters"])
    );
    // 404 USER_NOT_FOUND (detail & aksi).
    let r = app
        .get(&format!("/admin/users/{ghost}"))
        .token(&support.token)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(
        r.json,
        json!({"code":"USER_NOT_FOUND","message":"User not found"})
    );
    app.post(&format!("/admin/users/{ghost}/revoke-sessions"))
        .token(&support.token)
        .json(&json!({"reason": "tes tidak ada"}))
        .send()
        .await
        .assert_status(404);

    // Pencarian: substring username (case-insensitive), filter status, bentuk memberships.
    let needle = u.email.split('@').next().unwrap().to_uppercase();
    let r = app
        .get(&format!("/admin/users?search={needle}&itemsPerPage=5"))
        .token(&owner.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let d = &r.json["data"];
    assert_eq!(d["total"], json!(1));
    assert_eq!(d["pageNumber"], json!(1));
    assert_eq!(d["itemsPerPage"], json!(5));
    let row = &d["result"][0];
    assert_eq!(
        row.as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "uuid",
            "name",
            "email",
            "status",
            "emailVerifiedAt",
            "memberships"
        ]
    );
    assert_eq!(row["email"], json!(u.email));
    assert_eq!(row["status"], json!("ACTIVE"));
    assert_eq!(row["emailVerifiedAt"], Value::Null);
    assert_eq!(
        row["memberships"],
        json!([{
            "company": {"uuid": app.scalar::<String>(&format!("SELECT uuid::text FROM company WHERE id = {company}")).await, "name": app.scalar::<String>(&format!("SELECT name FROM company WHERE id = {company}")).await, "status": "ACTIVE"},
            "role": "ADMIN",
            "membershipStatus": "ACTIVE",
            "isDefault": true,
            "isOwner": true
        }])
    );
    let r = app
        .get("/admin/users?status=DISABLED&itemsPerPage=50")
        .token(&owner.token)
        .send()
        .await;
    r.assert_status(200);
    let ids: Vec<&str> = r.json["data"]["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["status"].as_str().unwrap())
        .collect();
    assert!(!ids.is_empty() && ids.iter().all(|s| *s == "DISABLED"));
    assert!(
        r.json["data"]["result"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["uuid"] == json!(other.uuid)),
        "user DISABLED terbaru ada di halaman pertama (urut id menurun)"
    );
    // Tanpa search = semua user, terbaru di atas.
    let r = app.get("/admin/users").token(&owner.token).send().await;
    assert_eq!(r.json["data"]["itemsPerPage"], json!(20));
    assert_eq!(r.json["data"]["result"][0]["uuid"], json!(other.uuid));

    // Detail: + hasGoogleLink + aksi operator terakhir (maks. 20, terbaru dulu).
    let r = app
        .get(&format!("/admin/users/{}", u.uuid))
        .token(&finance.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["hasGoogleLink"], json!(false));
    assert_eq!(r.json["data"]["recentOperatorActions"], json!([]));
    app.post(&format!("/admin/users/{}/revoke-sessions", u.uuid))
        .token(&support.token)
        .json(&json!({"reason": "cabut dulu"}))
        .send()
        .await
        .assert_status(201);
    let r = app
        .get(&format!("/admin/users/{}", u.uuid))
        .token(&finance.token)
        .send()
        .await;
    let recent = r.json["data"]["recentOperatorActions"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(recent.len(), 1);
    assert_eq!(
        recent[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "uuid",
            "action",
            "reason",
            "createdAt",
            "companyId",
            "operator"
        ]
    );
    assert_eq!(recent[0]["action"], json!("user.revoke_sessions"));
    assert_eq!(recent[0]["reason"], json!("cabut dulu"));
    assert_eq!(recent[0]["operator"], json!({"name": "IT Operator"}));
    let keys: Vec<&str> = r.json["data"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "uuid",
            "name",
            "email",
            "status",
            "emailVerifiedAt",
            "memberships",
            "hasGoogleLink",
            "recentOperatorActions"
        ]
    );
}

#[tokio::test]
async fn account_actions() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let support = operator(&app, "SUPPORT").await;

    // ── verify-email: hanya dari INVITED ──
    let invited = user(&app, 2).await;
    let company = new_company(&app, None).await;
    member(&app, &invited, company, "STAFF").await;
    let url = |u: &U, tail: &str| format!("/admin/users/{}/{tail}", u.uuid);
    let r = app
        .post(&url(&invited, "verify-email"))
        .token(&support.token)
        .json(&json!({"reason": "  sudah dikonfirmasi  "}))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    assert_eq!(
        r.json,
        json!({"success": true, "data": {"uuid": invited.uuid, "status": "ACTIVE"}})
    );
    assert_eq!(user_status(&app, invited.id).await, 1);
    let verified: bool = app
        .scalar(&format!(
            r#"SELECT "emailVerifiedAt" IS NOT NULL FROM users WHERE id = {}"#,
            invited.id
        ))
        .await;
    assert!(verified);
    let rows = op_audit(&app, &invited.uuid).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "user.verify_email");
    assert_eq!(rows[0].2, Some(json!({"status": "INVITED"})));
    assert_eq!(rows[0].3.as_ref().unwrap()["status"], json!("ACTIVE"));
    assert!(rows[0].3.as_ref().unwrap()["emailVerifiedAt"].is_string());
    assert_eq!(rows[0].4.as_deref(), Some("sudah dikonfirmasi"));
    let r = app
        .post(&url(&invited, "verify-email"))
        .token(&support.token)
        .json(&json!({"reason": "sekali lagi"}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(
        r.json,
        json!({"code":"NOT_PENDING_VERIFICATION","message":"User is not pending email verification"})
    );
    // Gagal = tidak ada baris operatorAuditLog baru.
    assert_eq!(op_audit(&app, &invited.uuid).await.len(), 1);

    // ── disable / enable ──
    let u = user(&app, 1).await;
    member(&app, &u, company, "STAFF").await;
    app.exec(&format!(
        r#"INSERT INTO "userConnected" ("userId", status, os, "macAddress") VALUES ({}, 1, 'win', 'AA:BB');"#,
        u.id
    ))
    .await;
    let r = app
        .post(&url(&u, "enable"))
        .token(&support.token)
        .json(&json!({"reason": "sudah aktif kok"}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], json!("NOT_DISABLED"));
    let r = app
        .post(&url(&u, "disable"))
        .token(&support.token)
        .json(&json!({"reason": "melanggar aturan"}))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    assert_eq!(
        r.json,
        json!({"success": true, "data": {"uuid": u.uuid, "status": "DISABLED"}})
    );
    assert_eq!(user_status(&app, u.id).await, 3);
    let revoked: bool = app
        .scalar(&format!(
            r#"SELECT "tokensValidAfter" IS NOT NULL FROM users WHERE id = {}"#,
            u.id
        ))
        .await;
    assert!(revoked, "semua token dicabut");
    let r = app
        .post(&url(&u, "disable"))
        .token(&support.token)
        .json(&json!({"reason": "dua kali"}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(
        r.json,
        json!({"code":"ALREADY_DISABLED","message":"User is already disabled"})
    );
    let deleted = user(&app, 0).await;
    let r = app
        .post(&url(&deleted, "disable"))
        .token(&support.token)
        .json(&json!({"reason": "sudah terhapus"}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], json!("USER_DELETED"));
    let r = app
        .post(&url(&u, "enable"))
        .token(&support.token)
        .json(&json!({"reason": "aktifkan lagi"}))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(
        r.json,
        json!({"success": true, "data": {"uuid": u.uuid, "status": "ACTIVE"}})
    );
    let acts: Vec<String> = op_audit(&app, &u.uuid)
        .await
        .into_iter()
        .map(|r| r.0)
        .collect();
    assert_eq!(acts, ["user.disable", "user.enable"]);
    // Pengguna DISABLED/DELETED tidak bisa dikirimi reset password.
    let r = app
        .post(&url(&deleted, "send-reset-password"))
        .token(&support.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(
        r.json,
        json!({"code":"USER_NOT_ACTIVE","message":"User is disabled or deleted"})
    );

    // ── disable keluarkan dari divisi ──
    let div: i32 = sqlx::query_scalar(
        r#"INSERT INTO division ("companyId", name) VALUES ($1, $2) RETURNING id"#,
    )
    .bind(company)
    .bind(format!("it-div-{}", &uniq()[..12]))
    .fetch_one(app.db())
    .await
    .unwrap();
    app.exec(&format!(
        r#"INSERT INTO "divisionMember" ("divisionId", "userId", "companyId", role)
           VALUES ({div}, {}, {company}, 'MEMBER'::"DivisionMemberRole")"#,
        u.id
    ))
    .await;
    app.post(&url(&u, "disable"))
        .token(&support.token)
        .json(&json!({"reason": "keluar divisi"}))
        .send()
        .await
        .assert_status(201);
    let left: bool = app
        .scalar(&format!(
            r#"SELECT "deletedAt" IS NOT NULL FROM "divisionMember" WHERE "userId" = {} AND "divisionId" = {div}"#,
            u.id
        ))
        .await;
    assert!(left, "keanggotaan divisi di-soft-delete");
    app.post(&url(&u, "enable"))
        .token(&support.token)
        .json(&json!({"reason": "aktifkan lagi"}))
        .send()
        .await
        .assert_status(201);

    // ── enable menghormati batas kursi MANUAL ──
    let seat_company = new_company(&app, None).await;
    app.exec(&format!(
        r#"UPDATE company SET "maxUsers" = 1 WHERE id = {seat_company};
           INSERT INTO subscriptions ("companyId", "pricingTierId", "startDate", "endDate", status, "maxUsers", "billingSource")
           VALUES ({seat_company}, (SELECT id FROM "pricingTiers" ORDER BY id LIMIT 1), NOW() - interval '1 day',
                   NOW() + interval '1 year', 'ACTIVE'::"SubscriptionStatus", 1, 'MANUAL'::"SubscriptionBillingSource");"#
    ))
    .await;
    let holder = user(&app, 1).await;
    member(&app, &holder, seat_company, "ADMIN").await;
    let off = user(&app, 3).await;
    member(&app, &off, seat_company, "STAFF").await;
    let r = app
        .post(&url(&off, "enable"))
        .token(&support.token)
        .json(&json!({"reason": "coba aktifkan"}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], json!("SEAT_LIMIT_REACHED"));
    assert_eq!(user_status(&app, off.id).await, 3, "tulisan di-rollback");

    // ── ganti email ──
    let owner_user = user(&app, 1).await;
    let owned = new_company(&app, Some(owner_user.id)).await;
    member(&app, &owner_user, owned, "SUPER_ADMIN").await;
    app.exec(&format!(
        r#"UPDATE users SET "googleId" = 'g{}' WHERE id = {}"#,
        uniq(),
        owner_user.id
    ))
    .await;
    let r = app
        .patch(&url(&owner_user, "email"))
        .token(&support.token)
        .json(&json!({"email": "bukan-email", "reason": "x"}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "email must be an email",
            "reason must be longer than or equal to 5 characters"
        ])
    );
    let r = app
        .patch(&url(&owner_user, "email"))
        .token(&support.token)
        .json(&json!({"email": owner_user.email.to_uppercase(), "reason": "email sama"}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(
        r.json,
        json!({"code":"SAME_EMAIL","message":"The new email is the same as the current one"})
    );
    let r = app
        .patch(&url(&owner_user, "email"))
        .token(&support.token)
        .json(&json!({"email": u.email.to_uppercase(), "reason": "dipakai orang"}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(
        r.json,
        json!({"code":"EMAIL_IN_USE","message":"Email is already used by another account"})
    );
    let new_email = format!("baru{}@it.test", uniq());
    let r = app
        .patch(&url(&owner_user, "email"))
        .token(&support.token)
        .json(
            &json!({"email": format!("  {}  ", new_email.to_uppercase()), "reason": "salah ketik"}),
        )
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(
        r.json,
        json!({"success": true, "data": {"uuid": owner_user.uuid, "email": new_email, "warnings": ["GOOGLE_SIGNIN_NEW_ADDRESS"]}})
    );
    let (name, reg, revoked): (String, Option<String>, bool) = sqlx::query_as(
        r#"SELECT u.username, c.register_email, u."tokensValidAfter" IS NOT NULL
             FROM users u JOIN company c ON c."ownerUserId" = u.id WHERE u.id = $1"#,
    )
    .bind(owner_user.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(name, new_email);
    assert_eq!(reg.as_deref(), Some(new_email.as_str()));
    assert!(revoked, "sesi lama dicabut");
    let rows = op_audit(&app, &owner_user.uuid).await;
    assert_eq!(rows[0].0, "user.change_email");
    assert_eq!(rows[0].2, Some(json!({"email": owner_user.email})));
    assert_eq!(rows[0].3, Some(json!({"email": new_email})));
    // Email > 50 karakter: company tidak diubah + peringatan.
    let long = format!("{}@it.test", "a".repeat(60));
    let r = app
        .patch(&url(&owner_user, "email"))
        .token(&support.token)
        .json(&json!({"email": long, "reason": "email panjang"}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.json["data"]["warnings"],
        json!([
            "OWNER_EMAIL_TOO_LONG_FOR_COMPANY_RECORD",
            "GOOGLE_SIGNIN_NEW_ADDRESS"
        ])
    );
    let reg: Option<String> = app
        .scalar(&format!(
            "SELECT register_email FROM company WHERE id = {owned}"
        ))
        .await;
    assert_eq!(reg.as_deref(), Some(new_email.as_str()));

    // ── revoke-sessions ──
    let r = app
        .post(&url(&u, "revoke-sessions"))
        .token(&support.token)
        .json(&json!({"reason": "perangkat hilang"}))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(r.json, json!({"success": true, "data": {"uuid": u.uuid}}));
    let active: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "userConnected" WHERE "userId" = {} AND status = 1"#,
            u.id
        ))
        .await;
    assert_eq!(active, 0);

    // ── send-reset-password: audit dulu, lalu token + email (gagal kirim tanpa Mailtrap) ──
    let r = app
        .post(&url(&owner_user, "send-reset-password"))
        .token(&support.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(
        r.json,
        json!({"success": true, "data": {"uuid": owner_user.uuid}})
    );
    let rows = op_audit(&app, &owner_user.uuid).await;
    let last = rows.last().unwrap();
    assert_eq!(last.0, "user.send_reset_password");
    assert_eq!(last.2, Some(json!({"status": "ACTIVE"})));
    assert_eq!(last.4, None);
    let tokens: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "userForgotPassword" WHERE "userId" = {}"#,
            owner_user.id
        ))
        .await;
    assert_eq!(tokens, 1);
    // `reason` opsional, tapi bila ada tetap 5..500.
    let r = app
        .post(&url(&owner_user, "send-reset-password"))
        .token(&support.token)
        .json(&json!({"reason": "abc"}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!(["reason must be longer than or equal to 5 characters"])
    );

    // ── audit entitas (`auditLog`): pelaku = "Timemoz Support (<nama operator>)" ──
    let mut found = false;
    for _ in 0..50 {
        let n: i64 = app
            .scalar(&format!(
                r#"SELECT COUNT(*) FROM "auditLog"
                    WHERE "entityUuid" = '{}'::uuid AND "actorName" = 'Timemoz Support (IT Operator)'"#,
                u.uuid
            ))
            .await;
        if n > 0 {
            found = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(found, "auditLog users.update oleh operator tercatat");
}
