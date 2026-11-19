//! Uji integrasi server OAuth + PAT `mz-mcp-api` (`/.well-known/*`, `/mcp-api/oauth/*`,
//! `/mcp-api/tokens`, `/mcp-api/audit-logs`, guard token) terhadap Postgres nyata.
//! Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-mcp-api --test it_mcp_api_oauth`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::useless_format
)]

use axum::Router;
use mz_core::AppState;
use mz_testkit::{TestApp, TestResponse};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn build(state: &AppState) -> Router {
    mz_mcp_api::router(state).with_state(state.clone())
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn sha(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// RFC 7636 appendix B.
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const REDIRECT: &str = "https://client.example.com/cb";

fn msg(r: &TestResponse) -> (u16, Value) {
    (r.status.as_u16(), r.json["message"].clone())
}

async fn register(app: &TestApp) -> String {
    let r = app
        .post("/mcp-api/oauth/register")
        .json(&json!({ "client_name": "IT User Client", "redirect_uris": [REDIRECT, "http://localhost:8080/cb"] }))
        .send()
        .await;
    r.assert_status(201);
    r.json["client_id"].as_str().unwrap().to_owned()
}

/// Setujui consent sebagai `jwt`; kembalikan kode.
async fn approve(app: &TestApp, jwt: &str, client: &str) -> String {
    let r = app
        .post("/mcp-api/oauth/authorize")
        .token(jwt)
        .json(&json!({
            "approved": true, "response_type": "code", "client_id": client,
            "redirect_uri": REDIRECT, "code_challenge": CHALLENGE, "state": "xyz"
        }))
        .send()
        .await;
    r.assert_status(200);
    let url = r.json["redirect_to"].as_str().unwrap().to_owned();
    assert!(
        url.starts_with("https://client.example.com/cb?code="),
        "{url}"
    );
    assert!(url.ends_with("&state=xyz"), "{url}");
    url.split("code=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap()
        .to_owned()
}

fn tok_body(code: &str, client: &str) -> Value {
    json!({ "grant_type": "authorization_code", "code": code, "client_id": client,
            "redirect_uri": REDIRECT, "code_verifier": VERIFIER })
}

/// User STAFF baru di company fixture `company_id` dengan `status` user tertentu. -> (id, jwt)
async fn member_user(app: &TestApp, company_id: i32, status: i32) -> (i32, String) {
    let u = uniq();
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO users (username, password, name, status)
               VALUES ('mcp{u}', 'x', 'MCP IT', {status}) RETURNING id"#
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "userCompany" ("userId", "companyId", status, "idleTimeInSecond", "isDefault")
           VALUES ({id}, {company_id}, 1, 300, true);
           INSERT INTO "userAccessLevel" ("userId", "accessLevelId", "companyId", status)
           VALUES ({id}, (SELECT id FROM "accessLevel" WHERE code = 'STAFF' AND "isSystem" ORDER BY id LIMIT 1),
                   {company_id}, 1);"#
    ))
    .await;
    (id, app.token(id))
}

async fn seed_pat(app: &TestApp, user: i32, plain: &str) {
    app.exec(&format!(
        r#"INSERT INTO "mcpAccessToken" ("userId", name, "tokenHash", "tokenPrefix", status)
           VALUES ({user}, 'it', '{}', '{}', 1)"#,
        sha(plain),
        &plain[..12]
    ))
    .await;
}

#[tokio::test]
async fn discovery_and_guard() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let r = app
        .get("/.well-known/oauth-authorization-server")
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.json,
        json!({
            "issuer": "http://localhost:3000",
            "authorization_endpoint": "http://localhost:3000/mcp-api/oauth/authorize",
            "token_endpoint": "http://localhost:3000/mcp-api/oauth/token",
            "registration_endpoint": "http://localhost:3000/mcp-api/oauth/register",
            "revocation_endpoint": "http://localhost:3000/mcp-api/oauth/revoke",
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none"],
            "scopes_supported": ["read"]
        })
    );
    let r = app
        .get("/.well-known/oauth-protected-resource")
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({
            "resource": "http://localhost:3000",
            "authorization_servers": ["http://localhost:3000"],
            "bearer_methods_supported": ["header"],
            "scopes_supported": ["read"]
        })
    );

    // PatAuthGuard: tanpa token -> 401 + WWW-Authenticate
    let r = app.get("/mcp-api/users/me").send().await;
    assert_eq!(msg(&r), (401, json!("Missing Bearer token")));
    assert_eq!(
        r.headers
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok()),
        Some(
            "Bearer resource_metadata=\"http://localhost:3000/.well-known/oauth-protected-resource\""
        )
    );
    let r = app
        .get("/mcp-api/users/me")
        .header("authorization", "Basic abc")
        .send()
        .await;
    assert_eq!(msg(&r), (401, json!("Missing Bearer token")));
    let r = app
        .get("/mcp-api/users/me")
        .token("jwt-like-token")
        .send()
        .await;
    assert_eq!(msg(&r), (401, json!("Invalid token type")));
    let r = app
        .get("/mcp-api/users/me")
        .token("mcp_pat_0000000000000000")
        .send()
        .await;
    assert_eq!(msg(&r), (401, json!("Invalid or expired token")));
    let r = app
        .get("/mcp-api/users/me")
        .token("mcp_oauth_0000000000000000")
        .send()
        .await;
    assert_eq!(msg(&r), (401, json!("Invalid or expired token")));
    // route lain / method lain: tidak pernah 5xx
    for p in [
        "/mcp-api/users",
        "/mcp-api/projects",
        "/mcp-api/tasks",
        "/mcp-api/reports/team",
        "/mcp-api/trackings/recent",
        "/mcp-api/calendar/events",
    ] {
        app.get(p).send().await.assert_status(401);
    }
}

#[tokio::test]
async fn token_validity_rules() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;
    let (uid, _) = member_user(&app, c.id, 1).await;
    let (dead_uid, _) = member_user(&app, c.id, 3).await;
    // token valid, milik user aktif -> lolos guard
    let good = format!("mcp_pat_{}", uniq());
    seed_pat(&app, uid, &good).await;
    let r = app.get("/mcp-api/users/me").token(&good).send().await;
    r.assert_status(200);

    // user dinonaktifkan -> token mati (audit C5)
    let t = format!("mcp_pat_{}", uniq());
    seed_pat(&app, dead_uid, &t).await;
    let r = app.get("/mcp-api/users/me").token(&t).send().await;
    assert_eq!(msg(&r), (401, json!("Invalid or expired token")));

    // dicabut / kedaluwarsa / status 0
    let mut bad: Vec<String> = Vec::new();
    for set in [
        r#""revokedAt" = NOW()"#,
        r#""expiresAt" = NOW() - interval '1 minute'"#,
        "status = 0",
    ] {
        let tok = format!("mcp_pat_{}", uniq());
        seed_pat(&app, uid, &tok).await;
        app.exec(&format!(
            r#"UPDATE "mcpAccessToken" SET {set} WHERE "tokenHash" = '{}'"#,
            sha(&tok)
        ))
        .await;
        bad.push(tok);
    }
    for tok in &bad {
        let r = app.get("/mcp-api/users/me").token(tok).send().await;
        assert_eq!(msg(&r), (401, json!("Invalid or expired token")), "{tok}");
    }
    // belum kedaluwarsa -> lolos
    let future = format!("mcp_pat_{}", uniq());
    seed_pat(&app, uid, &future).await;
    app.exec(&format!(
        r#"UPDATE "mcpAccessToken" SET "expiresAt" = NOW() + interval '1 day' WHERE "tokenHash" = '{}'"#,
        sha(&future)
    ))
    .await;
    app.get("/mcp-api/users/me")
        .token(&future)
        .send()
        .await
        .assert_status(200);

    // user aktif tanpa keanggotaan company: lolos guard, ditolak 403 oleh resolveMcpCaller
    let nc: i32 = app
        .scalar(&format!(
            r#"INSERT INTO users (username, password, name, status) VALUES ('mcpnc{}', 'x', 'NC', 1) RETURNING id"#,
            uniq()
        ))
        .await;
    let tok = format!("mcp_pat_{}", uniq());
    seed_pat(&app, nc, &tok).await;
    let r = app.get("/mcp-api/users/me").token(&tok).send().await;
    assert_eq!(msg(&r), (403, json!("No company found for user")));
}

#[tokio::test]
async fn pat_lifecycle_and_audit_logs() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;
    let jwt = c.staff.token.clone();

    // JwtAuthGuard
    app.get("/mcp-api/tokens").send().await.assert_status(401);
    app.post("/mcp-api/tokens")
        .json(&json!({ "name": "x" }))
        .send()
        .await
        .assert_status(401);

    // validasi DTO
    let r = app
        .post("/mcp-api/tokens")
        .token(&jwt)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "name must be shorter than or equal to 255 characters",
            "name should not be empty",
            "name must be a string"
        ])
    );

    // create (201): token tampil sekali, disimpan sebagai hash
    let r = app
        .post("/mcp-api/tokens")
        .token(&jwt)
        .json(&json!({ "name": "IT PAT" }))
        .send()
        .await;
    r.assert_status(201);
    let plain = r.json["token"].as_str().unwrap().to_owned();
    let uuid = r.json["uuid"].as_str().unwrap().to_owned();
    assert!(
        plain.starts_with("mcp_pat_") && plain.len() == 40,
        "{plain}"
    );
    assert_eq!(r.json["prefix"], json!(&plain[..12]));
    assert_eq!(r.json["name"], json!("IT PAT"));
    let stored: String = app
        .scalar(&format!(
            r#"SELECT "tokenHash" FROM "mcpAccessToken" WHERE uuid = '{uuid}'::uuid"#
        ))
        .await;
    assert_eq!(stored, sha(&plain));
    assert_ne!(stored, plain);

    // list: tanpa token mentah
    let r = app.get("/mcp-api/tokens").token(&jwt).send().await;
    r.assert_status(200);
    r.assert_no_sql_error();
    let item = r.json["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["uuid"] == json!(uuid))
        .unwrap();
    assert_eq!(item["name"], json!("IT PAT"));
    assert_eq!(item["scopes"], json!(["read"]));
    assert!(item.get("token").is_none() && item.get("tokenHash").is_none());
    assert_eq!(item["revokedAt"], Value::Null);

    // PAT dipakai (user di company fixture) + log audit
    let r = app.get("/mcp-api/users/me").token(&plain).send().await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["uuid"], json!(c.staff.uuid));
    assert_eq!(r.json["data"]["company"]["uuid"], json!(c.uuid));
    let r = app
        .get("/mcp-api/projects/not-a-uuid")
        .token(&plain)
        .send()
        .await;
    r.assert_status(400);
    // log di latar: tunggu
    let mut logs = Value::Null;
    for _ in 0..40 {
        let r = app
            .get(&format!("/mcp-api/audit-logs?tokenUuid={uuid}"))
            .token(&jwt)
            .send()
            .await;
        r.assert_status(200);
        r.assert_no_sql_error();
        if r.json["data"].as_array().is_some_and(|a| a.len() >= 2) {
            logs = r.json.clone();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let data = logs["data"].as_array().unwrap();
    assert!(data.iter().any(|l| l["path"] == json!("/mcp-api/users/me")
        && l["method"] == json!("GET")
        && l["statusCode"] == json!(200)
        && l["token"]["uuid"] == json!(uuid)));
    assert!(data.iter().any(|l| l["statusCode"] == json!(400)));
    assert_eq!(logs["meta"]["page"], json!(1));
    assert_eq!(logs["meta"]["perPage"], json!(50));
    assert!(logs["meta"]["total"].as_i64().unwrap() >= 2);
    // log milik user lain tidak bocor; tokenUuid asing -> kosong
    let r = app
        .get(&format!("/mcp-api/audit-logs?tokenUuid={uuid}"))
        .token(&c.staff2.token)
        .send()
        .await;
    assert_eq!(r.json["data"], json!([]));
    assert_eq!(r.json["meta"]["total"], json!(0));
    // validasi query
    let r = app
        .get("/mcp-api/audit-logs?perPage=999&page=0&dateFrom=x&tokenUuid=y")
        .token(&jwt)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "tokenUuid must be a UUID",
            "dateFrom must be a valid ISO 8601 date string",
            "page must not be less than 1",
            "perPage must not be greater than 200"
        ])
    );
    // filter tanggal
    app.get("/mcp-api/audit-logs?dateFrom=2001-01-01&dateTo=2001-01-02")
        .token(&jwt)
        .send()
        .await
        .assert_status(200);

    // revoke: milik user lain -> 404; uuid rusak -> 400; sukses -> token mati
    let r = app
        .delete(&format!("/mcp-api/tokens/{uuid}"))
        .token(&c.staff2.token)
        .send()
        .await;
    assert_eq!(msg(&r), (404, json!("Token not found")));
    app.delete("/mcp-api/tokens/nope")
        .token(&jwt)
        .send()
        .await
        .assert_status(400);
    let r = app
        .delete(&format!("/mcp-api/tokens/{uuid}"))
        .token(&jwt)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json, json!({ "success": true }));
    let r = app.get("/mcp-api/users/me").token(&plain).send().await;
    assert_eq!(msg(&r), (401, json!("Invalid or expired token")));
    // sudah dicabut -> 404 lagi
    let r = app
        .delete(&format!("/mcp-api/tokens/{uuid}"))
        .token(&jwt)
        .send()
        .await;
    r.assert_status(404);
}

#[tokio::test]
async fn oauth_register_authorize_token_refresh_revoke() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;
    let jwt = c.staff.token.clone();

    // register (handler dipakai bersama admin-mcp): aturan redirect_uri
    let r = app
        .post("/mcp-api/oauth/register")
        .json(&json!({ "redirect_uris": ["http://example.com/cb"] }))
        .send()
        .await;
    assert_eq!(
        msg(&r),
        (
            400,
            json!(
                "redirect_uri must use HTTPS or a custom scheme (except localhost): http://example.com/cb"
            )
        )
    );
    let client = register(&app).await;

    // authorize GET: validasi + redirect ke consent frontend
    let r = app.get("/mcp-api/oauth/authorize").send().await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"][0],
        json!("response_type must be one of the following values: code")
    );
    let q = format!(
        "response_type=code&client_id={client}&redirect_uri=https%3A%2F%2Fclient.example.com%2Fcb&code_challenge={CHALLENGE}&state=s1&scope=read"
    );
    let r = app
        .get(&format!("/mcp-api/oauth/authorize?{q}"))
        .send()
        .await;
    r.assert_status(302);
    let loc = r
        .headers
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        loc,
        format!(
            "http://localhost:3001/mcp-consent?client_id={client}&redirect_uri=https%3A%2F%2Fclient.example.com%2Fcb&code_challenge={CHALLENGE}&code_challenge_method=S256&response_type=code&state=s1&scope=read"
        )
    );
    // redirect_uri tak terdaftar / client asing / method bukan S256
    let bad = format!(
        "response_type=code&client_id={client}&redirect_uri=https%3A%2F%2Fevil.example.com%2Fcb&code_challenge={CHALLENGE}"
    );
    let r = app
        .get(&format!("/mcp-api/oauth/authorize?{bad}"))
        .send()
        .await;
    assert_eq!(
        msg(&r),
        (400, json!("redirect_uri not registered for this client"))
    );
    let unknown = format!(
        "response_type=code&client_id={}&redirect_uri=https%3A%2F%2Fclient.example.com%2Fcb&code_challenge=x",
        uuid::Uuid::new_v4()
    );
    let r = app
        .get(&format!("/mcp-api/oauth/authorize?{unknown}"))
        .send()
        .await;
    assert_eq!(msg(&r), (400, json!("Unknown client_id")));
    let r = app
        .get(&format!(
            "/mcp-api/oauth/authorize?{q}&code_challenge_method=plain"
        ))
        .send()
        .await;
    r.assert_status(400);

    // authorize POST: butuh JWT; validasi; penolakan; persetujuan
    app.post("/mcp-api/oauth/authorize")
        .json(&json!({}))
        .send()
        .await
        .assert_status(401);
    let r = app
        .post("/mcp-api/oauth/authorize")
        .token(&jwt)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"][0],
        json!("approved must be a boolean value")
    );
    let r = app
        .post("/mcp-api/oauth/authorize")
        .token(&jwt)
        .json(&json!({
            "approved": false, "response_type": "code", "client_id": client,
            "redirect_uri": REDIRECT, "code_challenge": CHALLENGE, "state": "st"
        }))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.json,
        json!({ "redirect_to": "https://client.example.com/cb?error=access_denied&state=st" })
    );
    let r = app
        .post("/mcp-api/oauth/authorize")
        .token(&jwt)
        .json(&json!({
            "approved": true, "response_type": "code", "client_id": client,
            "redirect_uri": "https://evil.example.com/cb", "code_challenge": CHALLENGE
        }))
        .send()
        .await;
    assert_eq!(
        msg(&r),
        (400, json!("redirect_uri not registered for this client"))
    );

    let code = approve(&app, &jwt, &client).await;
    let stored: (String, String, Vec<String>) = sqlx::query_as(
        r#"SELECT "codeChallenge", "codeChallengeMethod", scopes FROM "mcpOAuthCode" WHERE code = $1"#,
    )
    .bind(&code)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(
        stored,
        (
            CHALLENGE.to_owned(),
            "S256".to_owned(),
            vec!["read".to_owned()]
        )
    );

    // token: validasi & kegagalan
    let r = app
        .post("/mcp-api/oauth/token")
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "grant_type must be one of the following values: authorization_code, refresh_token",
            "client_id must be a UUID"
        ])
    );
    let r = app
        .post("/mcp-api/oauth/token")
        .json(&json!({ "grant_type": "authorization_code", "client_id": client }))
        .send()
        .await;
    assert_eq!(msg(&r), (400, json!("code is required")));
    let r = app
        .post("/mcp-api/oauth/token")
        .json(&json!({ "grant_type": "authorization_code", "client_id": client, "code": "c" }))
        .send()
        .await;
    assert_eq!(msg(&r), (400, json!("redirect_uri is required")));
    let r = app
        .post("/mcp-api/oauth/token")
        .json(&json!({ "grant_type": "authorization_code", "client_id": client, "code": "c", "redirect_uri": REDIRECT }))
        .send()
        .await;
    assert_eq!(msg(&r), (400, json!("code_verifier is required")));
    let r = app
        .post("/mcp-api/oauth/token")
        .json(&json!({ "grant_type": "refresh_token", "client_id": client }))
        .send()
        .await;
    assert_eq!(msg(&r), (400, json!("refresh_token is required")));
    let r = app
        .post("/mcp-api/oauth/token")
        .json(&tok_body("nope", &client))
        .send()
        .await;
    assert_eq!(msg(&r), (401, json!("Invalid authorization code")));
    // client_id / redirect_uri berbeda
    let other_client = register(&app).await;
    let r = app
        .post("/mcp-api/oauth/token")
        .json(&tok_body(&code, &other_client))
        .send()
        .await;
    assert_eq!(msg(&r), (401, json!("client_id mismatch")));
    let mut b = tok_body(&code, &client);
    b["redirect_uri"] = json!("http://localhost:8080/cb");
    let r = app.post("/mcp-api/oauth/token").json(&b).send().await;
    assert_eq!(msg(&r), (401, json!("redirect_uri mismatch")));
    // PKCE salah -> kode TIDAK terpakai
    let mut b = tok_body(&code, &client);
    b["code_verifier"] = json!("x".repeat(43));
    let r = app.post("/mcp-api/oauth/token").json(&b).send().await;
    assert_eq!(msg(&r), (401, json!("PKCE verification failed")));

    // pertukaran sukses (juga lewat form-urlencoded, seperti klien OAuth sungguhan)
    let form = format!(
        "grant_type=authorization_code&code={code}&client_id={client}&redirect_uri=https%3A%2F%2Fclient.example.com%2Fcb&code_verifier={VERIFIER}"
    );
    let r = app
        .post("/mcp-api/oauth/token")
        .raw("application/x-www-form-urlencoded", form)
        .send()
        .await;
    r.assert_status(200);
    let access = r.json["access_token"].as_str().unwrap().to_owned();
    let refresh = r.json["refresh_token"].as_str().unwrap().to_owned();
    assert!(access.starts_with("mcp_oauth_") && access.len() == 10 + 32);
    assert!(refresh.starts_with("mcp_refresh_") && refresh.len() == 12 + 48);
    assert_eq!(r.json["token_type"], json!("Bearer"));
    assert_eq!(r.json["expires_in"], json!(86400));
    assert_eq!(r.json["refresh_token_expires_in"], json!(31_536_000));
    assert_eq!(r.json["scope"], json!("read"));
    // tersimpan sebagai hash
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "mcpOAuthAccessToken" WHERE "tokenHash" = '{}' AND "tokenPrefix" = '{}'"#,
            sha(&access),
            &access[..16]
        ))
        .await;
    assert_eq!(n, 1);
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "mcpOAuthRefreshToken" WHERE "tokenHash" = '{}' AND "tokenPrefix" = '{}'"#,
            sha(&refresh),
            &refresh[..16]
        ))
        .await;
    assert_eq!(n, 1);
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "mcpOAuthAccessToken" WHERE "tokenHash" = '{access}'"#
        ))
        .await;
    assert_eq!(n, 0, "token tidak boleh tersimpan plain");

    // kode sekali pakai
    let r = app
        .post("/mcp-api/oauth/token")
        .json(&tok_body(&code, &client))
        .send()
        .await;
    assert_eq!(msg(&r), (401, json!("Authorization code already used")));

    // kode kedaluwarsa
    let code2 = approve(&app, &jwt, &client).await;
    app.exec(&format!(
        r#"UPDATE "mcpOAuthCode" SET "expiresAt" = NOW() - interval '1 second' WHERE code = '{code2}'"#
    ))
    .await;
    let r = app
        .post("/mcp-api/oauth/token")
        .json(&tok_body(&code2, &client))
        .send()
        .await;
    assert_eq!(msg(&r), (401, json!("Authorization code expired")));

    // access token OAuth dipakai di endpoint data + log OAuth ditulis
    let r = app.get("/mcp-api/users/me").token(&access).send().await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["uuid"], json!(c.staff.uuid));
    let mut n = 0_i64;
    for _ in 0..40 {
        n = app
            .scalar(&format!(
                r#"SELECT count(*) FROM "mcpOAuthAccessTokenLog" WHERE "clientId" = '{client}'::uuid AND path = '/mcp-api/users/me' AND "statusCode" = 200 AND "clientName" = 'IT User Client'"#
            ))
            .await;
        if n > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(n, 1);

    // refresh: format, client, rotasi (sekali pakai)
    let r = app
        .post("/mcp-api/oauth/token")
        .json(
            &json!({ "grant_type": "refresh_token", "client_id": client, "refresh_token": "nope" }),
        )
        .send()
        .await;
    assert_eq!(msg(&r), (401, json!("Invalid refresh token format")));
    let r = app
        .post("/mcp-api/oauth/token")
        .json(&json!({ "grant_type": "refresh_token", "client_id": client, "refresh_token": "mcp_refresh_zz" }))
        .send()
        .await;
    assert_eq!(msg(&r), (401, json!("Invalid refresh token")));
    let r = app
        .post("/mcp-api/oauth/token")
        .json(&json!({ "grant_type": "refresh_token", "client_id": other_client, "refresh_token": refresh }))
        .send()
        .await;
    assert_eq!(msg(&r), (401, json!("client_id mismatch")));
    let r = app
        .post("/mcp-api/oauth/token")
        .json(&json!({ "grant_type": "refresh_token", "client_id": client, "refresh_token": refresh }))
        .send()
        .await;
    r.assert_status(200);
    let access2 = r.json["access_token"].as_str().unwrap().to_owned();
    let refresh2 = r.json["refresh_token"].as_str().unwrap().to_owned();
    assert_ne!(access2, access);
    assert_ne!(refresh2, refresh);
    let r = app
        .post("/mcp-api/oauth/token")
        .json(&json!({ "grant_type": "refresh_token", "client_id": client, "refresh_token": refresh }))
        .send()
        .await;
    assert_eq!(msg(&r), (401, json!("Refresh token already used")));
    // refresh dicabut / kedaluwarsa
    app.exec(&format!(
        r#"UPDATE "mcpOAuthRefreshToken" SET "revokedAt" = NOW() WHERE "tokenHash" = '{}'"#,
        sha(&refresh2)
    ))
    .await;
    let r = app
        .post("/mcp-api/oauth/token")
        .json(&json!({ "grant_type": "refresh_token", "client_id": client, "refresh_token": refresh2 }))
        .send()
        .await;
    assert_eq!(msg(&r), (401, json!("Refresh token revoked")));

    // revoke: JWT wajib; format; bukan milik pemanggil -> 404; sukses -> token mati
    app.post("/mcp-api/oauth/revoke")
        .json(&json!({ "token": access2 }))
        .send()
        .await
        .assert_status(401);
    let r = app
        .post("/mcp-api/oauth/revoke")
        .token(&jwt)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!(["token should not be empty", "token must be a string"])
    );
    let r = app
        .post("/mcp-api/oauth/revoke")
        .token(&jwt)
        .json(&json!({ "token": "mcp_pat_x" }))
        .send()
        .await;
    assert_eq!(msg(&r), (400, json!("Invalid token format")));
    let r = app
        .post("/mcp-api/oauth/revoke")
        .token(&c.staff2.token)
        .json(&json!({ "token": access2 }))
        .send()
        .await;
    assert_eq!(msg(&r), (404, json!("Token not found")));
    app.get("/mcp-api/users/me")
        .token(&access2)
        .send()
        .await
        .assert_status(200);
    let r = app
        .post("/mcp-api/oauth/revoke")
        .token(&jwt)
        .json(&json!({ "token": access2 }))
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json, json!({ "revoked": true }));
    let r = app.get("/mcp-api/users/me").token(&access2).send().await;
    assert_eq!(msg(&r), (401, json!("Invalid or expired token")));

    // access token kedaluwarsa & milik user nonaktif -> mati
    let r = app
        .post("/mcp-api/oauth/token")
        .json(&json!({ "grant_type": "refresh_token", "client_id": client, "refresh_token": refresh }))
        .send()
        .await;
    assert_eq!(r.status.as_u16(), 401);
    let code3 = approve(&app, &jwt, &client).await;
    let r = app
        .post("/mcp-api/oauth/token")
        .json(&tok_body(&code3, &client))
        .send()
        .await;
    let a3 = r.json["access_token"].as_str().unwrap().to_owned();
    app.exec(&format!(
        r#"UPDATE "mcpOAuthAccessToken" SET "expiresAt" = NOW() - interval '1 second' WHERE "tokenHash" = '{}'"#,
        sha(&a3)
    ))
    .await;
    let r = app.get("/mcp-api/users/me").token(&a3).send().await;
    assert_eq!(msg(&r), (401, json!("Invalid or expired token")));
}

#[tokio::test]
async fn oauth_disabled_user_token_dies() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let c = app.company().await;
    let (uid, jwt) = member_user(&app, c.id, 1).await;
    let client = register(&app).await;
    let code = approve(&app, &jwt, &client).await;
    let r = app
        .post("/mcp-api/oauth/token")
        .json(&tok_body(&code, &client))
        .send()
        .await;
    r.assert_status(200);
    let access = r.json["access_token"].as_str().unwrap().to_owned();
    app.get("/mcp-api/users/me")
        .token(&access)
        .send()
        .await
        .assert_status(200);
    app.exec(&format!("UPDATE users SET status = 3 WHERE id = {uid}"))
        .await;
    let r = app.get("/mcp-api/users/me").token(&access).send().await;
    assert_eq!(msg(&r), (401, json!("Invalid or expired token")));
}
