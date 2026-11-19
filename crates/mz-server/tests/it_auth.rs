//! Uji integrasi `mz-auth` (18 route) + `mz-account` (confirm, forgot/change password,
//! enable/disable) terhadap Postgres & Redis nyata.
//!
//! Tidak tercakup (butuh jaringan keluar): token exchange Google di `/auth/google-redirect`,
//! PayPal `createSubscription` (jalur register berbayar sukses), pengiriman email, Mixpanel, Discord.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::Duration;

use mz_testkit::{JWT_SECRET, TestApp};
use serde_json::{Value, json};

const PASSWORD: &str = "Passw0rd!x";
/// Tier TIME gratis (`maxUsers` 2) dan tier FULL berbayar (PayPal plan terisi).
const TIER_FREE_TIME: &str = "2e64a475-4af6-4d8a-9db3-fdfc2bff6401";
const TIER_FREE_SALES: &str = "7339bdef-07ae-4f0a-9ceb-c586be557b60";
const TIER_PAID: &str = "75b22fb4-e243-4c9f-8d6b-7d23eea22a64";

async fn new_app() -> Option<TestApp> {
    TestApp::new(mz_server::app).await
}

static IP_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// IP unik per request (TestApp per-test memulai urutan IP dari 1 yang sama di Redis bersama).
fn nip() -> String {
    static BASE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    let base = *BASE.get_or_init(|| uuid::Uuid::new_v4().as_u128() as u32 % 60_000);
    let n = base + IP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("172.{}.{}.{}", 16 + (n >> 16) % 16, (n >> 8) & 255, n & 255)
}

fn post<'a>(app: &'a TestApp, uri: &str) -> mz_testkit::RequestBuilder<'a> {
    app.post(uri).ip(&nip())
}

fn get<'a>(app: &'a TestApp, uri: &str) -> mz_testkit::RequestBuilder<'a> {
    app.get(uri).ip(&nip())
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_owned()
}

fn email(tag: &str) -> String {
    format!("it.{tag}.{}@example.test", uniq())
}

fn hash(plain: &str) -> String {
    bcrypt::hash(plain, 4).unwrap()
}

struct Acct {
    username: String,
    id: i32,
    uuid: String,
    company_id: i32,
}

async fn register_body(app: &TestApp, username: &str, tier: &str, trial: bool) -> Value {
    let res = post(app, "/auth/register")
        .json(&json!({
            "username": username, "name": "Uji Register", "companyName": "DEFAULT",
            "address": "Jl. Uji 1", "handphone": "0812", "roles": "Owner", "total_employees": "1-10",
            "pricingUuid": tier, "password": PASSWORD, "isTrial": trial,
            "attribution": { "utm_source": "it" },
        }))
        .send()
        .await;
    res.assert_status(200);
    res.json
}

/// Daftar via route lalu aktifkan + set password dikenal lewat SQL.
async fn register_active(app: &TestApp, tier: &str) -> Acct {
    let username = email("reg");
    let body = register_body(app, &username, tier, true).await;
    assert_eq!(body["success"], json!(true), "{body}");
    let (id, uuid): (i32, String) =
        sqlx::query_as("SELECT id, uuid::text FROM users WHERE username = $1")
            .bind(&username)
            .fetch_one(app.db())
            .await
            .unwrap();
    sqlx::query("UPDATE users SET password = $2, status = 1 WHERE id = $1")
        .bind(id)
        .bind(hash(PASSWORD))
        .execute(app.db())
        .await
        .unwrap();
    let company_id: i32 =
        sqlx::query_scalar(r#"SELECT "companyId" FROM "userCompany" WHERE "userId" = $1"#)
            .bind(id)
            .fetch_one(app.db())
            .await
            .unwrap();
    Acct {
        username,
        id,
        uuid,
        company_id,
    }
}

async fn login(app: &TestApp, username: &str, password: &str) -> Value {
    let res = post(app, "/auth/login")
        .json(&json!({ "username": username, "password": password }))
        .send()
        .await;
    res.assert_status(200);
    res.json
}

/// Tunggu entri audit (flush berjalan di `tokio::spawn` sesudah respons).
async fn audit_rows(app: &TestApp, sql: &str) -> i64 {
    for _ in 0..40 {
        let n: i64 = app.scalar(sql).await;
        if n > 0 {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    0
}

/// Email reset dikirim di `tokio::spawn`; ia menandai token `status = 0` (siap dipakai).
async fn wait_token_usable(app: &TestApp, user_id: i32) {
    for _ in 0..50 {
        let n: i64 = app
            .scalar(&format!(r#"SELECT COUNT(*) FROM "userForgotPassword" WHERE "userId" = {user_id} AND status = 0"#))
            .await;
        if n > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("token reset tidak pernah berstatus 0");
}

// ───────────────────────────── register / confirm ─────────────────────────────

#[tokio::test]
async fn register_creates_company_and_audit() {
    let Some(app) = new_app().await else { return };
    let username = email("reg");
    let body = register_body(&app, &username, TIER_FREE_SALES, true).await;
    assert_eq!(body["success"], json!(true), "{body}");

    let (uid, uuid, status): (i32, String, i32) =
        sqlx::query_as("SELECT id, uuid::text, status FROM users WHERE username = $1")
            .bind(&username)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(status, 2, "user baru berstatus INVITED");
    // company + seed + subscription + role SUPER_ADMIN
    let cid: i32 = app
        .scalar(&format!(
            r#"SELECT "companyId" FROM "userCompany" WHERE "userId" = {uid}"#
        ))
        .await;
    for (table, min) in [
        ("subscriptions", 1),
        (r#""companyLeaveType""#, 6),
        (r#""customerType""#, 1),
        ("uom", 1),
        (r#""leavePolicy""#, 1),
        (r#""projectCompany""#, 1),
    ] {
        let n: i64 = app
            .scalar(&format!(
                r#"SELECT COUNT(*) FROM {table} WHERE "companyId" = {cid}"#
            ))
            .await;
        assert!(n >= min, "{table} seed company: {n}");
    }
    let role: String = app
        .scalar(&format!(
            r#"SELECT al.code FROM "userAccessLevel" ual JOIN "accessLevel" al ON al.id = ual."accessLevelId"
               WHERE ual."userId" = {uid}"#
        ))
        .await;
    assert_eq!(role, "SUPER_ADMIN");

    // audit: users create, company/userCompany/userAccessLevel companions
    let users_audit = audit_rows(
        &app,
        &format!(r#"SELECT COUNT(*) FROM "auditLog" WHERE "entityType"::text = 'employee' AND "entityId" = {uid} AND action::text = 'create'"#),
    )
    .await;
    assert!(
        users_audit >= 1,
        "auditLog create users untuk register tidak ada (uuid {uuid})"
    );
    let total: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "auditLog" WHERE "companyId" = {cid}"#
        ))
        .await;
    assert!(total >= 1, "auditLog tanpa baris untuk company {cid}");

    // duplikat & validasi
    let dup = register_body(&app, &username.to_uppercase(), TIER_FREE_SALES, true).await;
    assert_eq!(dup["success"], json!(false), "{dup}");
    assert_eq!(dup["message"], "Username already exists");
    let nf = register_body(
        &app,
        &email("nf"),
        "00000000-0000-4000-8000-000000000000",
        true,
    )
    .await;
    assert_eq!(nf["message"], "Pricing not found");
    post(&app, "/auth/register")
        .json(&json!({"username": "bukan-email"}))
        .send()
        .await
        .assert_status(400);
}

#[tokio::test]
async fn register_paid_fails_before_payment_without_paypal() {
    let Some(app) = new_app().await else { return };
    let username = email("paid");
    let body = register_body(&app, &username, TIER_PAID, false).await;
    assert_eq!(body["success"], json!(false), "{body}");
    let n: i64 = app
        .scalar(&format!(
            "SELECT COUNT(*) FROM users WHERE username = '{username}'"
        ))
        .await;
    assert_eq!(
        n, 0,
        "register berbayar yang gagal tidak boleh meninggalkan user"
    );
    // tier tanpa PayPal plan + bukan trial
    let body = register_body(&app, &email("paid2"), TIER_FREE_TIME, false).await;
    assert_eq!(
        body["message"],
        "PayPal plan not configured for this pricing tier"
    );
}

#[tokio::test]
async fn confirm_register_and_check_verification_and_resend() {
    let Some(app) = new_app().await else { return };
    let username = email("cr");
    assert_eq!(
        register_body(&app, &username, TIER_FREE_TIME, true).await["success"],
        json!(true)
    );
    let uuid: String = app
        .scalar(&format!(
            "SELECT uuid::text FROM users WHERE username = '{username}'"
        ))
        .await;

    let cv = |u: String| {
        let app = &app;
        async move {
            let r = get(app, &format!("/auth/check-verification?username={u}"))
                .send()
                .await;
            r.assert_status(200);
            r.json
        }
    };
    assert_eq!(
        cv(username.clone()).await["data"]["isVerified"],
        json!(false)
    );
    get(&app, "/auth/check-verification")
        .send()
        .await
        .assert_status(200);
    assert_eq!(
        get(&app, "/auth/check-verification").send().await.json["message"],
        "Username is required"
    );

    // resend (selalu respons generik)
    let r = post(&app, "/auth/resend-confirmation")
        .json(&json!({"username": username}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["success"], json!(true));
    post(&app, "/auth/resend-confirmation")
        .json(&json!({"username": email("ghost")}))
        .send()
        .await
        .assert_ok();

    // token salah
    let r = post(&app, "/auth/confirm-register")
        .json(&json!({"username": username, "token": hash("salah")}))
        .send()
        .await;
    assert_eq!(r.json["message"], "Invalid or expired confirmation token");
    let r = post(&app, "/auth/confirm-register")
        .json(&json!({"username": email("ghost"), "token": "x"}))
        .send()
        .await;
    assert_eq!(r.json["message"], "User not found");

    // token benar = bcrypt(uuid)
    let r = post(&app, "/auth/confirm-register")
        .json(&json!({"username": username, "token": hash(&uuid)}))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["success"], json!(true), "{}", r.text());
    assert_eq!(
        cv(username.clone()).await["data"]["isVerified"],
        json!(true)
    );
    let verified: bool = app
        .scalar(&format!(
            r#"SELECT "emailVerifiedAt" IS NOT NULL FROM users WHERE username = '{username}'"#
        ))
        .await;
    assert!(verified);
    let again = post(&app, "/auth/confirm-register")
        .json(&json!({"username": username, "token": hash(&uuid)}))
        .send()
        .await;
    assert_eq!(again.json["message"], "Account is already confirmed");
    assert!(
        audit_rows(
            &app,
            &format!(r#"SELECT COUNT(*) FROM "auditLog" WHERE "entityType"::text = 'employee' AND action::text = 'update' AND "entityUuid" = '{uuid}'::uuid"#),
        )
        .await
            >= 1,
        "auditLog update users.status (confirm-register) tidak ada"
    );
}

#[tokio::test]
async fn confirm_with_bcrypt_token_and_invite_token() {
    let Some(app) = new_app().await else { return };
    let owner = register_active(&app, TIER_FREE_SALES).await;

    // user INVITED di company owner via register-invite
    let invite_code: String = app
        .scalar(&format!(
            r#"SELECT "inviteCode" FROM company WHERE id = {}"#,
            owner.company_id
        ))
        .await;
    // maxUsers tier ini = 2: undang satu user (owner + 1 = penuh)
    let mk_invitee = || async {
        let u = email("inv");
        let r = post(&app, "/auth/register-invite")
            .json(&json!({"username": u, "name": "Tamu Undangan", "password": PASSWORD, "inviteCode": invite_code}))
            .send()
            .await;
        (u, r)
    };
    let (u1, r) = mk_invitee().await;
    r.assert_status(200);
    assert_eq!(r.json["success"], json!(true), "{}", r.text());
    assert_eq!(r.json["data"]["username"], json!(u1));
    let uuid1: String = app
        .scalar(&format!(
            "SELECT uuid::text FROM users WHERE username = '{u1}'"
        ))
        .await;

    // salah password
    let r = post(&app, "/auth/confirm")
        .json(&json!({"token": hash(&uuid1), "username": u1, "password": "Salah-Banget1"}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["code"], "CONFIRM_WRONG_PASSWORD");
    // token tidak valid
    post(&app, "/auth/confirm")
        .json(&json!({"token": hash("lain"), "username": u1, "password": PASSWORD}))
        .send()
        .await
        .assert_status(404);
    // benar: aktivasi + projectUser
    let r = post(&app, "/auth/confirm")
        .json(&json!({"token": hash(&uuid1), "username": u1, "password": PASSWORD}))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["status"], json!(1));
    let status: i32 = app
        .scalar(&format!("SELECT status FROM users WHERE username = '{u1}'"))
        .await;
    assert_eq!(status, 1);

    // invite token (HMAC) untuk user INVITED lain: set password baru
    let uid_other = {
        let username = email("inv2");
        let id: i32 = sqlx::query_scalar(
            "INSERT INTO users (username, name, password, status) VALUES ($1, 'Lain', $2, 2) RETURNING id",
        )
        .bind(&username)
        .bind(hash("x"))
        .fetch_one(app.db())
        .await
        .unwrap();
        sqlx::query(r#"INSERT INTO "userCompany" ("userId", "companyId", status, "isDefault", "idleTimeInSecond") VALUES ($1, $2, 1, true, 180)"#)
            .bind(id).bind(owner.company_id).execute(app.db()).await.unwrap();
        sqlx::query(r#"INSERT INTO "userAccessLevel" ("userId", "accessLevelId", "companyId", status)
               SELECT $1, id, $2, 1 FROM "accessLevel" WHERE code = 'STAFF' AND "isSystem" = true AND status = 1 ORDER BY id LIMIT 1"#)
            .bind(id).bind(owner.company_id).execute(app.db()).await.unwrap();
        (id, username)
    };
    let uuid2: String = app
        .scalar(&format!(
            "SELECT uuid::text FROM users WHERE id = {}",
            uid_other.0
        ))
        .await;
    let tok = mz_account::invite_token::sign_invite_token(
        JWT_SECRET,
        &uuid2,
        chrono::Utc::now().timestamp_millis(),
    )
    .unwrap();
    // password < 8 → validasi
    post(&app, "/auth/confirm")
        .json(&json!({"token": tok, "username": uid_other.1, "password": "short"}))
        .send()
        .await
        .assert_status(400);
    let r = post(&app, "/auth/confirm")
        .json(&json!({"token": tok, "username": uid_other.1, "password": "BrandNew-Pass9"}))
        .send()
        .await;
    r.assert_status(200);
    // userAccessLevel tidak ada → role None; hanya jalur claim yang kita uji, activated=true
    // butuh role; respons boleh 500 hanya bila data setup tak lengkap.
    let st: i32 = app
        .scalar(&format!(
            "SELECT status FROM users WHERE id = {}",
            uid_other.0
        ))
        .await;
    assert_eq!(st, 1, "invite token harus mengaktifkan user: {}", r.text());
    let l = login(&app, &uid_other.1, "BrandNew-Pass9").await;
    assert_eq!(l["success"], json!(true), "{l}");
}

// ───────────────────────────── invite ─────────────────────────────

#[tokio::test]
async fn invite_info_and_register_invite_errors() {
    let Some(app) = new_app().await else { return };
    let owner = register_active(&app, TIER_FREE_TIME).await;
    let code: String = app
        .scalar(&format!(
            r#"SELECT "inviteCode" FROM company WHERE id = {}"#,
            owner.company_id
        ))
        .await;

    let r = get(&app, &format!("/auth/invite/{code}")).send().await;
    r.assert_status(200);
    assert_eq!(r.json["companyName"], json!("DEFAULT"));
    assert_eq!(r.json["isFull"], json!(false));
    let r = get(&app, "/auth/invite/tidakada").send().await;
    r.assert_status(404);
    assert_eq!(r.json["code"], "INVITE_INVALID");

    // email sudah terdaftar
    let r = post(&app, "/auth/register-invite")
        .json(&json!({"username": owner.username, "name": "X", "password": PASSWORD, "inviteCode": code}))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "EMAIL_TAKEN");

    // sukses lalu company penuh (maxUsers 2)
    let ok = email("inv");
    let r = post(&app, "/auth/register-invite")
        .json(&json!({"username": ok, "name": "Anggota", "password": PASSWORD, "inviteCode": code}))
        .send()
        .await;
    assert_eq!(r.json["success"], json!(true), "{}", r.text());
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "userAccessLevel" WHERE "companyId" = {} AND "userId" = (SELECT id FROM users WHERE username = '{ok}')"#,
            owner.company_id
        ))
        .await;
    assert_eq!(n, 1);
    assert_eq!(
        get(&app, &format!("/auth/invite/{code}")).send().await.json["isFull"],
        json!(true)
    );
    let r = post(&app, "/auth/register-invite")
        .json(&json!({"username": email("late"), "name": "Telat", "password": PASSWORD, "inviteCode": code}))
        .send()
        .await;
    r.assert_status(403);
    assert_eq!(r.json["code"], "COMPANY_FULL");
    assert!(audit_rows(&app, &format!(r#"SELECT COUNT(*) FROM "auditLog" WHERE "companyId" = {} AND action::text='create'"#, owner.company_id)).await >= 1);

    // validasi DTO
    post(&app, "/auth/register-invite")
        .json(&json!({}))
        .send()
        .await
        .assert_status(400);
}

// ───────────────────────────── login / me / logout / web-session ─────────────────────────────

#[tokio::test]
async fn login_me_logout_web_session() {
    let Some(app) = new_app().await else { return };
    let a = register_active(&app, TIER_FREE_TIME).await;

    // password salah / user tak ada
    let r = login(&app, &a.username, "salahsalah").await;
    assert_eq!(r["code"], "INVALID_CREDENTIALS");
    assert_eq!(
        login(&app, &email("ghost"), "salahsalah").await["code"],
        "INVALID_CREDENTIALS"
    );

    // sukses (tanpa mac): bentuk lengkap
    let r = login(&app, &a.username.to_uppercase(), PASSWORD).await;
    assert_eq!(r["success"], json!(true), "{r}");
    let d = &r["data"];
    assert_eq!(d["accessLevel"], "SUPER_ADMIN");
    assert_eq!(d["hasCreateOrganization"], json!(false));
    assert_eq!(d["company"]["company"]["id"], json!(a.company_id));
    assert!(d["company"]["company"]["trial"].is_array());
    let token = d["accessToken"].as_str().unwrap().to_owned();

    // /auth/me
    let me = get(&app, "/auth/me").token(&token).send().await;
    me.assert_ok();
    assert_eq!(me.data()["id"], json!(a.id));
    assert!(me.data().get("subscriptionLock").is_some());
    get(&app, "/auth/me").send().await.assert_status(401);

    // device login (mac+os): sid + userConnected
    let r = post(&app, "/auth/login")
        .json(&json!({"username": a.username, "password": PASSWORD, "macAddress": "AA:BB:CC:01", "os": "Linux", "platform": "desktop"}))
        .send()
        .await;
    r.assert_ok();
    let dev_token = r.data()["accessToken"].as_str().unwrap().to_owned();
    let claims = mz_core::jwt::decode_claims(JWT_SECRET, &dev_token).unwrap();
    let sid = claims["sid"].as_str().unwrap().to_owned();
    let n: i64 = app
        .scalar(&format!(r#"SELECT COUNT(*) FROM "userConnected" WHERE "userId" = {} AND status = 1 AND sid = '{sid}'::uuid"#, a.id))
        .await;
    assert_eq!(n, 1);
    // device kedua mematikan yang pertama
    let r2 = post(&app, "/auth/login")
        .json(&json!({"username": a.username, "password": PASSWORD, "macAddress": "AA:BB:CC:02", "os": "Linux"}))
        .send()
        .await;
    r2.assert_ok();
    let active: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "userConnected" WHERE "userId" = {} AND status = 1"#,
            a.id
        ))
        .await;
    assert_eq!(active, 1);
    // mac ketiga + subscription inactive → ditolak
    // (token device pertama sudah tidak dikenal)
    post(&app, "/auth/web-session")
        .token(&dev_token)
        .send()
        .await
        .assert_status(401);

    // web-session tanpa sid = token sama; dengan sid valid = token baru tanpa sid
    let ws = post(&app, "/auth/web-session").token(&token).send().await;
    ws.assert_ok();
    assert_eq!(ws.data()["accessToken"], json!(token));
    let sid2 = mz_core::jwt::decode_claims(JWT_SECRET, r2.data()["accessToken"].as_str().unwrap())
        .unwrap()["sid"]
        .as_str()
        .unwrap()
        .to_owned();
    let tok_sid2 = app.token_with(json!({"userId": a.id, "sid": sid2}));
    let ws = post(&app, "/auth/web-session")
        .token(&tok_sid2)
        .send()
        .await;
    ws.assert_ok();
    let new_claims =
        mz_core::jwt::decode_claims(JWT_SECRET, ws.data()["accessToken"].as_str().unwrap())
            .unwrap();
    assert!(new_claims.get("sid").is_none());

    // logout: blacklist + userConnected nonaktif
    post(&app, "/auth/logout")
        .token(&tok_sid2)
        .send()
        .await
        .assert_ok();
    let bl: i64 = app.scalar("SELECT COUNT(*) FROM \"jwtBlackList\"").await;
    let _ = bl;
    get(&app, "/auth/me")
        .token(&tok_sid2)
        .send()
        .await
        .assert_status(401);
    let active: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "userConnected" WHERE "userId" = {} AND status = 1"#,
            a.id
        ))
        .await;
    assert_eq!(active, 0);
    // logout tanpa sid (jalur mac header kosong → None) tetap sukses
    post(&app, "/auth/logout")
        .token(&token)
        .send()
        .await
        .assert_ok();
    get(&app, "/auth/me")
        .token(&token)
        .send()
        .await
        .assert_status(401);
    post(&app, "/auth/logout").send().await.assert_status(401);
}

#[tokio::test]
async fn login_states_and_validation() {
    let Some(app) = new_app().await else { return };
    let a = register_active(&app, TIER_FREE_TIME).await;
    let set = |s: i32| {
        let app = &app;
        let id = a.id;
        async move {
            sqlx::query("UPDATE users SET status = $2 WHERE id = $1")
                .bind(id)
                .bind(s)
                .execute(app.db())
                .await
                .unwrap();
        }
    };
    set(2).await;
    assert_eq!(
        login(&app, &a.username, PASSWORD).await["code"],
        "ACCOUNT_NOT_ACTIVE"
    );
    set(3).await;
    assert_eq!(
        login(&app, &a.username, PASSWORD).await["code"],
        "ACCOUNT_DEACTIVATED"
    );
    set(1).await;

    // subscription inactive (jalur device)
    app.exec(&format!(
        r#"UPDATE subscriptions SET status = 'SUSPENDED' WHERE "companyId" = {}"#,
        a.company_id
    ))
    .await;
    let r = post(&app, "/auth/login")
        .json(&json!({"username": a.username, "password": PASSWORD, "macAddress": "DE:AD:BE:EF", "os": "Win"}))
        .send()
        .await;
    assert_eq!(r.json["code"], "SUBSCRIPTION_INACTIVE", "{}", r.text());

    // user tanpa company aktif
    app.exec(&format!(
        r#"UPDATE "userCompany" SET status = 0 WHERE "userId" = {}"#,
        a.id
    ))
    .await;
    assert_eq!(
        login(&app, &a.username, PASSWORD).await["code"],
        "NO_ACTIVE_COMPANY"
    );

    post(&app, "/auth/login")
        .json(&json!({"username": format!("x{}", uniq())}))
        .send()
        .await
        .assert_status(400);
    post(&app, "/auth/login")
        .json(&json!({"username": a.username, "password": PASSWORD, "platform": "pc"}))
        .send()
        .await
        .assert_status(400);

    // lockout setelah 10 percobaan gagal
    let victim = email("lock");
    let mut locked = false;
    for _ in 0..12 {
        let r = login(&app, &victim, "salahsalah").await;
        if r["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("Too many failed"))
        {
            locked = true;
            break;
        }
    }
    assert!(locked, "lockout login tidak aktif");
}

// ───────────────────────────── change / forgot password ─────────────────────────────

#[tokio::test]
async fn change_password_flow_and_audit() {
    let Some(app) = new_app().await else { return };
    let a = register_active(&app, TIER_FREE_TIME).await;
    let token = login(&app, &a.username, PASSWORD).await["data"]["accessToken"]
        .as_str()
        .unwrap()
        .to_owned();
    let call = |old: &str, new: &str, conf: &str| {
        let app = &app;
        let token = token.clone();
        let body = json!({"oldPassword": old, "newPassword": new, "confirmNewPassword": conf});
        async move {
            post(app, "/auth/change-password")
                .token(&token)
                .json(&body)
                .send()
                .await
        }
    };
    assert_eq!(
        call(PASSWORD, "NewPass-123", "beda-beda1").await.json["message"],
        "New password and confirmation do not match"
    );
    assert_eq!(
        call("salahsalah", "NewPass-123", "NewPass-123").await.json["message"],
        "Old password is incorrect"
    );
    assert_eq!(
        call(PASSWORD, PASSWORD, PASSWORD).await.json["message"],
        "New password must be different from the old password"
    );
    post(&app, "/auth/change-password")
        .token(&token)
        .json(&json!({"oldPassword": PASSWORD}))
        .send()
        .await
        .assert_status(400);

    let r = call(PASSWORD, "NewPass-123", "NewPass-123").await;
    r.assert_ok();
    assert_eq!(r.data()["id"], json!(a.id));
    let new_token = r.data()["accessToken"].as_str().unwrap().to_owned();
    // token lama (iat lebih lama dari detik pencabutan) dicabut, token baru jalan
    let stale =
        app.token_with(json!({"userId": a.id, "iat": chrono::Utc::now().timestamp() - 120}));
    get(&app, "/auth/me")
        .token(&stale)
        .send()
        .await
        .assert_status(401);
    get(&app, "/auth/me")
        .token(&new_token)
        .send()
        .await
        .assert_ok();
    assert_eq!(
        login(&app, &a.username, PASSWORD).await["code"],
        "INVALID_CREDENTIALS"
    );
    assert_eq!(
        login(&app, &a.username, "NewPass-123").await["success"],
        json!(true)
    );

    // Kebijakan audit `users` = allowlist (username, name, status): ganti password murni
    // tidak mengubah kolom ber-audit → TIDAK ada baris (Node sama). Tunggu flush lalu pastikan.
    tokio::time::sleep(Duration::from_millis(800)).await;
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "auditLog" WHERE "entityType"::text = 'employee' AND "entityId" = {} AND action::text = 'update'"#,
            a.id
        ))
        .await;
    assert_eq!(
        n, 0,
        "change-password tidak boleh menulis auditLog (password di luar allowlist)"
    );
    // nilai password tidak boleh bocor ke audit
    let leaked: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "auditLog" WHERE "entityId" = {} AND ("before"::text LIKE '%$2%' OR "after"::text LIKE '%$2%')"#,
            a.id
        ))
        .await;
    assert_eq!(leaked, 0, "hash password bocor ke auditLog");
}

#[tokio::test]
async fn forgot_password_flow() {
    let Some(app) = new_app().await else { return };
    let a = register_active(&app, TIER_FREE_TIME).await;

    // generate: respons generik utk user ada / tidak ada
    for u in [a.username.clone(), email("ghost")] {
        let r = post(&app, "/auth/forgot-password/generate")
            .json(&json!({"username": u}))
            .send()
            .await;
        r.assert_ok();
    }
    post(&app, "/auth/forgot-password/generate")
        .json(&json!({"username": format!("x{}", uniq())}))
        .send()
        .await
        .assert_status(400);
    let (token, status): (String, i32) = sqlx::query_as(
        r#"SELECT uuid::text, status FROM "userForgotPassword" WHERE "userId" = $1 ORDER BY id DESC LIMIT 1"#,
    )
    .bind(a.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert!(status == 0 || status == 1, "status token reset: {status}");
    wait_token_usable(&app, a.id).await;

    let old_token =
        app.token_with(json!({"userId": a.id, "iat": chrono::Utc::now().timestamp() - 120}));
    get(&app, "/auth/me")
        .token(&old_token)
        .send()
        .await
        .assert_ok();
    // token salah
    let r = post(&app, "/auth/forgot-password")
        .json(&json!({"username": a.username, "password": "Reset-Pass-1", "forgotPasswordUuid": uuid::Uuid::new_v4().to_string()}))
        .send()
        .await;
    assert_eq!(r.json["message"], "Invalid or expired token");
    post(&app, "/auth/forgot-password")
        .json(&json!({"username": a.username, "password": "Reset-Pass-1", "forgotPasswordUuid": "bukan-uuid"}))
        .send()
        .await
        .assert_status(400);

    let r = post(&app, "/auth/forgot-password")
        .json(&json!({"username": a.username, "password": "Reset-Pass-1", "forgotPasswordUuid": token}))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["username"], json!(a.username));
    // token sekali pakai, sesi lama dicabut, password baru berlaku
    let again = post(&app, "/auth/forgot-password")
        .json(&json!({"username": a.username, "password": "Reset-Pass-2", "forgotPasswordUuid": token}))
        .send()
        .await;
    assert_eq!(again.json["success"], json!(false));
    get(&app, "/auth/me")
        .token(&old_token)
        .send()
        .await
        .assert_status(401);
    assert_eq!(
        login(&app, &a.username, "Reset-Pass-1").await["success"],
        json!(true)
    );
    // reset password juga tidak ber-audit (password di luar allowlist); status token bukan model ber-audit

    // token kedaluwarsa
    app.exec(&format!(
        r#"DELETE FROM "userForgotPassword" WHERE "userId" = {}"#,
        a.id
    ))
    .await;
    post(&app, "/auth/forgot-password/generate")
        .json(&json!({"username": a.username}))
        .send()
        .await
        .assert_ok();
    wait_token_usable(&app, a.id).await;
    app.exec(&format!(r#"UPDATE "userForgotPassword" SET "expiredAt" = NOW() - interval '1 hour' WHERE "userId" = {}"#, a.id)).await;
    let t2: String = app
        .scalar(&format!(r#"SELECT uuid::text FROM "userForgotPassword" WHERE "userId" = {} ORDER BY id DESC LIMIT 1"#, a.id))
        .await;
    let r = post(&app, "/auth/forgot-password")
        .json(
            &json!({"username": a.username, "password": "Reset-Pass-3", "forgotPasswordUuid": t2}),
        )
        .send()
        .await;
    assert_eq!(r.json["message"], "Invalid or expired token");
    // generate lagi setelah kedaluwarsa membuat baris baru
    post(&app, "/auth/forgot-password/generate")
        .json(&json!({"username": a.username}))
        .send()
        .await
        .assert_ok();
    let n: i64 = app
        .scalar(&format!(r#"SELECT COUNT(*) FROM "userForgotPassword" WHERE "userId" = {} AND "expiredAt" > NOW()"#, a.id))
        .await;
    assert_eq!(n, 1);
}

// ───────────────────────────── onboarding ─────────────────────────────

#[tokio::test]
async fn create_organization() {
    let Some(app) = new_app().await else { return };
    let a = register_active(&app, TIER_FREE_TIME).await;
    let token = login(&app, &a.username, PASSWORD).await["data"]["accessToken"]
        .as_str()
        .unwrap()
        .to_owned();

    // cache /auth/me terisi dulu → harus di-invalidate
    let me = get(&app, "/auth/me").token(&token).send().await;
    me.assert_ok();

    post(&app, "/auth/onboarding/create-organization")
        .token(&token)
        .json(&json!({}))
        .send()
        .await
        .assert_status(400);
    post(&app, "/auth/onboarding/create-organization")
        .token(&token)
        .json(&json!({"name": "Org", "modules": ["BAD"]}))
        .send()
        .await
        .assert_status(400);
    post(&app, "/auth/onboarding/create-organization")
        .json(&json!({"name": "Org"}))
        .send()
        .await
        .assert_status(401);

    let r = post(&app, "/auth/onboarding/create-organization")
        .token(&token)
        .json(&json!({
            "name": "Org Baru", "websiteURL": "https://org.test", "register_total_employees": "11-50",
            "modules": ["TIME_TRACKING", "SALES_TRACKER"], "timeZone": "Asia/Jakarta",
        }))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["name"], "Org Baru");
    assert_eq!(r.data()["hasCreateOrgnanization"], json!(true));
    assert_eq!(
        r.data()["modules"],
        json!(["TIME_TRACKING", "SALES_TRACKER"])
    );
    let (tz, tt, st): (Option<String>, bool, bool) = sqlx::query_as(
        r#"SELECT "timeZone", "moduleTimeTrackingEnabled", "moduleSalesTrackerEnabled" FROM company WHERE id = $1"#,
    )
    .bind(a.company_id)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(tz.as_deref(), Some("Asia/Jakarta"));
    assert!(tt && st);
    let q: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "companyQuickstart" WHERE "companyId" = {}"#,
            a.company_id
        ))
        .await;
    assert_eq!(q, 1);
    let me = get(&app, "/auth/me").token(&token).send().await;
    me.assert_ok();

    // panggilan kedua: bukan first → timeZone tidak berubah
    let r = post(&app, "/auth/onboarding/create-organization")
        .token(&token)
        .json(&json!({"name": "Org Kedua", "timeZone": "Asia/Tokyo", "websiteURL": null}))
        .send()
        .await;
    r.assert_ok();
    let tz: Option<String> = sqlx::query_scalar(r#"SELECT "timeZone" FROM company WHERE id = $1"#)
        .bind(a.company_id)
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(tz.as_deref(), Some("Asia/Jakarta"));
    assert!(audit_rows(&app, &format!(r#"SELECT COUNT(*) FROM "auditLog" WHERE "companyId" = {} AND "entityType"::text = 'company' AND action::text = 'update'"#, a.company_id)).await >= 1);
}

// ───────────────────────────── mz-account: enable / disable ─────────────────────────────

#[tokio::test]
async fn account_enable_disable() {
    let Some(app) = new_app().await else { return };
    let a = register_active(&app, TIER_FREE_SALES).await;
    // anggota: tier maxUsers 2 → owner + 1
    let code: String = app
        .scalar(&format!(
            r#"SELECT "inviteCode" FROM company WHERE id = {}"#,
            a.company_id
        ))
        .await;
    let member = email("mem");
    let r = post(&app, "/auth/register-invite")
        .json(&json!({"username": member, "name": "Anggota", "password": PASSWORD, "inviteCode": code}))
        .send()
        .await;
    assert_eq!(r.json["success"], json!(true), "{}", r.text());
    let (mid, muuid): (i32, String) =
        sqlx::query_as("SELECT id, uuid::text FROM users WHERE username = $1")
            .bind(&member)
            .fetch_one(app.db())
            .await
            .unwrap();
    app.exec(&format!("UPDATE users SET status = 1 WHERE id = {mid}"))
        .await;
    let mtoken = login(&app, &member, PASSWORD).await["data"]["accessToken"]
        .as_str()
        .unwrap()
        .to_owned();
    get(&app, "/auth/me")
        .token(&mtoken)
        .send()
        .await
        .assert_ok();

    let audit = mz_audit::AuditHandle::detached(app.db().clone());
    let d = mz_account::user_ops::disable(&app.state, &audit, &muuid, a.id)
        .await
        .unwrap();
    assert_eq!(d["success"], json!(true));
    let st: i32 = app
        .scalar(&format!("SELECT status FROM users WHERE id = {mid}"))
        .await;
    assert_eq!(st, 3);
    get(&app, "/auth/me")
        .token(&mtoken)
        .send()
        .await
        .assert_status(401);

    let e = mz_account::user_ops::enable(&app.state, &audit, &muuid, a.id)
        .await
        .unwrap();
    assert_eq!(e["success"], json!(true), "{e}");
    let st: i32 = app
        .scalar(&format!("SELECT status FROM users WHERE id = {mid}"))
        .await;
    assert_eq!(st, 1);
    assert!(audit_rows(&app, &format!(r#"SELECT COUNT(*) FROM "auditLog" WHERE "entityType"::text = 'employee' AND "entityId" = {mid} AND action::text = 'update'"#)).await >= 1);

    // requester di luar company → galat (bukan 5xx SQL)
    let outsider = register_active(&app, TIER_FREE_TIME).await;
    assert!(
        mz_account::user_ops::disable(&app.state, &audit, &muuid, outsider.id)
            .await
            .is_err()
    );
    let _ = a.uuid;
}

// ───────────────────────────── google (tanpa jaringan) ─────────────────────────────

#[tokio::test]
async fn google_redirects_and_db_paths() {
    let Some(app) = new_app().await else { return };
    for (uri, action) in [
        ("/auth/google?macAddress=AA&os=Win&utm_source=it", "login"),
        ("/auth/google/register", "register"),
    ] {
        let r = get(&app, uri).send().await;
        r.assert_status(302);
        let loc = r
            .headers
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(loc.starts_with("https://accounts.google.com/"), "{loc}");
        assert!(
            loc.contains("client_id=test-client") && loc.contains("state="),
            "{loc}"
        );
        assert!(
            loc.contains(&format!(
                "redirect_uri={}",
                "http%3A%2F%2Fbe.test%2Fauth%2Fgoogle-redirect"
            )),
            "{loc}"
        );
        assert!(r.headers.get("set-cookie").is_some());
        let _ = action;
    }

    // callback: code dengan state/cookie tidak valid → redirect FE (CSRF)
    let r = get(&app, "/auth/google-redirect?code=abc&state=bogus")
        .send()
        .await;
    r.assert_status(302);
    assert!(
        r.headers
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("/auth/google-callback?success=false")
    );
    // user menolak
    get(&app, "/auth/google-redirect?error=access_denied")
        .send()
        .await
        .assert_status(401);

    // jalur DB (setelah pertukaran token) dipanggil langsung
    use mz_auth::service::oauth::{GoogleUser, callback_location};
    let audit = mz_audit::AuditHandle::detached(app.db().clone());
    let g = email("goog");
    let guser = GoogleUser {
        email: g.clone(),
        first_name: Some("Gina".into()),
        last_name: Some("Google".into()),
    };
    // login tanpa akun + tanpa mac → auto-register
    let loc = callback_location(
        &app.state,
        &audit,
        &guser,
        Some(r#"{"action":"login","macAddress":"","os":""}"#),
    )
    .await
    .unwrap();
    // GOOGLE_DEFAULT_PRICING_UUID tidak diset → tier pertama; sukses atau pesan terkontrol
    assert!(
        loc.starts_with("http://fe.test/auth/google-callback?success="),
        "{loc}"
    );
    if loc.contains("success=true") {
        let n: i64 = app
            .scalar(&format!(
                "SELECT COUNT(*) FROM users WHERE username = '{g}' AND status = 1"
            ))
            .await;
        assert_eq!(n, 1);
        // login kedua (akun aktif) + device
        let loc = callback_location(
            &app.state,
            &audit,
            &guser,
            Some(r#"{"action":"login","macAddress":"GG:01","os":"Win","platform":"desktop"}"#),
        )
        .await
        .unwrap();
        assert!(loc.contains("success=true&token="), "{loc}");
        // register ulang → akun sudah ada
        let loc = callback_location(&app.state, &audit, &guser, Some(r#"{"action":"register"}"#))
            .await
            .unwrap();
        assert!(loc.contains("success=false"), "{loc}");
    } else {
        panic!("register Google gagal: {loc}");
    }
    // login akun tak ada dengan mac → tidak auto-register
    let ghost = GoogleUser {
        email: email("gh"),
        first_name: Some("G".into()),
        last_name: None,
    };
    let loc = callback_location(
        &app.state,
        &audit,
        &ghost,
        Some(r#"{"action":"login","macAddress":"X","os":"Y"}"#),
    )
    .await
    .unwrap();
    assert!(loc.contains("success=false"), "{loc}");
}

// ───────────────────────────── rate limit ─────────────────────────────

#[tokio::test]
async fn login_rate_limit_per_ip() {
    let Some(app) = new_app().await else { return };
    let mut got_429 = false;
    let ip = nip();
    for _ in 0..70 {
        let r = post(&app, "/auth/login")
            .ip(&ip)
            .json(&json!({"username": "a@b.co", "password": "salahsalah"}))
            .send()
            .await;
        if r.status.as_u16() == 429 {
            got_429 = true;
            break;
        }
    }
    assert!(got_429, "limiter /auth/login tidak membatasi");
}
