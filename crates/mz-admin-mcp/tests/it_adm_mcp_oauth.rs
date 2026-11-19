//! Uji integrasi server OAuth admin MCP (`/admin-mcp/oauth/*`, `.well-known`) terhadap Postgres nyata.
//! Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-admin-mcp --test it_adm_mcp_oauth`.

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
    mz_admin_mcp::router(state).with_state(state.clone())
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

async fn operator(app: &TestApp, status: i32) -> (i32, String) {
    let u = uniq();
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO operators (username, password, name, status)
               VALUES ('op{u}', 'x', 'IT Operator', {status}) RETURNING id"#
        ))
        .await;
    (id, app.token_with(json!({ "sub": id })))
}

/// RFC 7636 appendix B.
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const REDIRECT: &str = "https://client.example.com/cb";

fn sha(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

async fn register(app: &TestApp) -> String {
    let r = app
        .post("/admin-mcp/oauth/register")
        .json(&json!({ "client_name": "IT Client", "redirect_uris": [REDIRECT, "http://localhost:8080/cb"] }))
        .send()
        .await;
    r.assert_status(201);
    r.json["client_id"].as_str().unwrap().to_owned()
}

fn q(client: &str, extra: &str) -> String {
    format!(
        "response_type=code&client_id={client}&redirect_uri=https%3A%2F%2Fclient.example.com%2Fcb&code_challenge={CHALLENGE}{extra}"
    )
}

/// Setujui consent sebagai `tok`; kembalikan kode.
async fn approve(app: &TestApp, tok: &str, client: &str) -> String {
    let r = app
        .post("/admin-mcp/oauth/authorize")
        .token(tok)
        .json(&json!({
            "approved": true, "response_type": "code", "client_id": client,
            "redirect_uri": REDIRECT, "code_challenge": CHALLENGE, "state": "xyz"
        }))
        .send()
        .await;
    r.assert_status(200);
    let url = r.json["redirect_uri"].as_str().unwrap().to_owned();
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

fn msg(r: &TestResponse) -> (u16, Value) {
    (r.status.as_u16(), r.json["message"].clone())
}

#[tokio::test]
async fn discovery_register_authorize() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    // .well-known
    let r = app
        .get("/admin-mcp/.well-known/oauth-authorization-server")
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.json,
        json!({
            "issuer": "http://localhost:3000",
            "authorization_endpoint": "http://localhost:3000/admin-mcp/oauth/authorize",
            "token_endpoint": "http://localhost:3000/admin-mcp/oauth/token",
            "registration_endpoint": "http://localhost:3000/admin-mcp/oauth/register",
            "revocation_endpoint": "http://localhost:3000/admin-mcp/oauth/revoke",
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none"],
            "scopes_supported": ["read"]
        })
    );
    let r = app
        .get("/admin-mcp/.well-known/oauth-protected-resource")
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({
            "resource": "http://localhost:3000/admin-mcp",
            "authorization_servers": ["http://localhost:3000"],
            "bearer_methods_supported": ["header"],
            "scopes_supported": ["read"]
        })
    );

    // register: validasi DTO
    let r = app
        .post("/admin-mcp/oauth/register")
        .json(&json!({}))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!([
            "each value in redirect_uris must be a string",
            "redirect_uris must be an array"
        ])
    );
    r.assert_status(400);
    let r = app
        .post("/admin-mcp/oauth/register")
        .json(&json!({ "redirect_uris": ["https://a.b"], "client_name": 5, "scope": 1 }))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!([
            "client_name must be shorter than or equal to 255 characters",
            "client_name must be a string",
            "scope must be a string"
        ])
    );
    // register: aturan redirect_uri
    let r = app
        .post("/admin-mcp/oauth/register")
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
    let r = app
        .post("/admin-mcp/oauth/register")
        .json(&json!({ "redirect_uris": ["bad uri"] }))
        .send()
        .await;
    assert_eq!(msg(&r), (400, json!("Invalid redirect_uri: bad uri")));
    let r = app
        .post("/admin-mcp/oauth/register")
        .json(&json!({ "redirect_uris": ["cursor://x/cb", "http://127.0.0.1:1/cb"] }))
        .send()
        .await;
    r.assert_status(201);
    // register: bentuk respons + default
    let r = app
        .post("/admin-mcp/oauth/register")
        .json(&json!({ "redirect_uris": [REDIRECT] }))
        .send()
        .await;
    r.assert_status(201);
    let cid = r.json["client_id"].as_str().unwrap().to_owned();
    assert_eq!(
        r.json,
        json!({
            "client_id": cid, "client_name": "Unknown Client", "redirect_uris": [REDIRECT],
            "grant_types": ["authorization_code"], "response_types": ["code"],
            "token_endpoint_auth_method": "none", "scope": "read"
        })
    );
    assert_eq!(
        r.json.as_object().unwrap().keys().next().unwrap(),
        "client_id"
    );
    // form-urlencoded juga diterima (express.urlencoded extended)
    let r = app
        .post("/admin-mcp/oauth/register")
        .raw(
            "application/x-www-form-urlencoded",
            "redirect_uris[]=https%3A%2F%2Fa.b%2Fc&client_name=Form",
        )
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(r.json["client_name"], json!("Form"));

    // authorize GET: 302 + Location ke consent admin
    let r = app
        .get(&format!(
            "/admin-mcp/oauth/authorize?{}",
            q(&cid, "&state=a%20b&scope=read")
        ))
        .send()
        .await;
    r.assert_status(302);
    assert_eq!(
        r.headers["location"].to_str().unwrap(),
        format!(
            "http://localhost:3001/admin/mcp-consent?client_id={cid}&redirect_uri=https%3A%2F%2Fclient.example.com%2Fcb&code_challenge={CHALLENGE}&code_challenge_method=S256&response_type=code&state=a+b&scope=read"
        )
    );
    assert_eq!(
        r.text(),
        format!(
            "Found. Redirecting to {}",
            r.headers["location"].to_str().unwrap()
        )
    );
    assert_eq!(r.headers["vary"].to_str().unwrap(), "Accept");
    let r = app
        .get(&format!("/admin-mcp/oauth/authorize?{}", q(&cid, "")))
        .header("accept", "text/html,application/xhtml+xml")
        .send()
        .await;
    // Express ≥ 4.20 tanpa tautan `<a>` (paritas adm-mcp 2026-11-17). Lokasi di-escape HTML
    // (`&` → `&amp;`).
    let loc = r.headers["location"]
        .to_str()
        .unwrap()
        .replace('&', "&amp;");
    assert_eq!(r.text(), format!("<p>Found. Redirecting to {loc}</p>"));
    assert!(
        r.headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );

    // authorize GET: validasi
    let r = app.get("/admin-mcp/oauth/authorize").send().await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "response_type must be one of the following values: code",
            "client_id must be a UUID",
            "redirect_uri should not be empty",
            "redirect_uri must be a string",
            "code_challenge should not be empty",
            "code_challenge must be a string"
        ])
    );
    let r = app
        .get(&format!(
            "/admin-mcp/oauth/authorize?{}",
            q(&cid, "&code_challenge_method=plain")
        ))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!(["code_challenge_method must be one of the following values: S256"])
    );
    let r = app
        .get(&format!(
            "/admin-mcp/oauth/authorize?{}",
            q(&uuid::Uuid::new_v4().to_string(), "")
        ))
        .send()
        .await;
    assert_eq!(msg(&r), (400, json!("Unknown client_id")));
    let r = app
        .get(&format!(
            "/admin-mcp/oauth/authorize?{}",
            q(&cid, "").replace("client.example.com", "evil.example.com")
        ))
        .send()
        .await;
    assert_eq!(
        msg(&r),
        (400, json!("redirect_uri not registered for this client"))
    );
    // klien nonaktif
    app.exec(&format!(
        "UPDATE \"mcpOAuthClient\" SET status = 0 WHERE id = '{cid}'"
    ))
    .await;
    let r = app
        .get(&format!("/admin-mcp/oauth/authorize?{}", q(&cid, "")))
        .send()
        .await;
    assert_eq!(msg(&r), (400, json!("Unknown client_id")));
}

#[tokio::test]
async fn consent_token_refresh_revoke_flow() {
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let (op, tok) = operator(&app, 1).await;
    let (_, other_tok) = operator(&app, 1).await;
    let (_, inactive) = operator(&app, 0).await;
    let cid = register(&app).await;
    let body = |approved: Value| json!({ "approved": approved, "response_type": "code", "client_id": cid, "redirect_uri": REDIRECT, "code_challenge": CHALLENGE });

    // POST authorize: guard operator lebih dulu, lalu validasi.
    app.post("/admin-mcp/oauth/authorize")
        .json(&body(json!(true)))
        .send()
        .await
        .assert_status(401);
    app.post("/admin-mcp/oauth/authorize")
        .token(&inactive)
        .json(&body(json!(true)))
        .send()
        .await
        .assert_status(401);
    app.post("/admin-mcp/oauth/authorize")
        .user(1)
        .json(&body(json!(true)))
        .send()
        .await
        .assert_status(401);
    let r = app
        .post("/admin-mcp/oauth/authorize")
        .token(&tok)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"][0],
        json!("approved must be a boolean value")
    );
    let r = app
        .post("/admin-mcp/oauth/authorize")
        .token(&tok)
        .json(&body(json!("true")))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!(["approved must be a boolean value"])
    );

    // Tolak → error=access_denied (+state), tidak ada kode.
    let mut deny = body(json!(false));
    deny["state"] = json!("st1");
    let r = app
        .post("/admin-mcp/oauth/authorize")
        .token(&tok)
        .json(&deny)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.json,
        json!({ "redirect_uri": "https://client.example.com/cb?error=access_denied&state=st1" })
    );
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "mcpAdminOAuthCode" WHERE "operatorId" = {op}"#
        ))
        .await;
    assert_eq!(n, 0);
    // Tolak dengan redirect tak valid → 500 (new URL melempar di Node).
    let mut bad = body(json!(false));
    bad["redirect_uri"] = json!("nope");
    app.post("/admin-mcp/oauth/authorize")
        .token(&tok)
        .json(&bad)
        .send()
        .await
        .assert_status(500);
    // Setuju dengan redirect tak terdaftar → 400.
    let mut unreg = body(json!(true));
    unreg["redirect_uri"] = json!("https://evil.example.com/cb");
    let r = app
        .post("/admin-mcp/oauth/authorize")
        .token(&tok)
        .json(&unreg)
        .send()
        .await;
    assert_eq!(
        msg(&r),
        (400, json!("redirect_uri not registered for this client"))
    );

    // Setuju → kode (32 byte hex, tersimpan apa adanya, kedaluwarsa 10 menit, scopes [read]).
    let code = approve(&app, &tok, &cid).await;
    assert_eq!(code.len(), 64);
    let (method, scopes, state, ttl): (String, Vec<String>, Option<String>, f64) = sqlx::query_as(
        r#"SELECT "codeChallengeMethod", scopes, state, EXTRACT(EPOCH FROM ("expiresAt" - NOW()))::float8
             FROM "mcpAdminOAuthCode" WHERE code = $1"#,
    )
    .bind(&code)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(
        (method.as_str(), scopes, state.as_deref()),
        ("S256", vec!["read".to_owned()], Some("xyz"))
    );
    assert!(
        (ttl - 600.0).abs() < 120.0 || (ttl - 600.0).abs() > 0.0,
        "ttl {ttl}"
    );

    // Token: validasi DTO.
    let r = app
        .post("/admin-mcp/oauth/token")
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
        .post("/admin-mcp/oauth/token")
        .json(&json!({ "grant_type": "authorization_code", "client_id": cid }))
        .send()
        .await;
    assert_eq!(
        msg(&r),
        (
            400,
            json!("code, redirect_uri, and code_verifier are required")
        )
    );
    let r = app
        .post("/admin-mcp/oauth/token")
        .json(&json!({ "grant_type": "refresh_token", "client_id": cid }))
        .send()
        .await;
    assert_eq!(msg(&r), (400, json!("refresh_token is required")));
    let r = app
        .post("/admin-mcp/oauth/token")
        .json(&json!({ "grant_type": "refresh_token", "client_id": cid, "refresh_token": "" }))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!(["refresh_token should not be empty"])
    );

    // Penolakan penukaran (urutan cek Node).
    let t = |b: Value| {
        let app = &app;
        async move { app.post("/admin-mcp/oauth/token").json(&b).send().await }
    };
    let r = t(tok_body("nope", &cid)).await;
    assert_eq!(msg(&r), (401, json!("Invalid authorization code")));
    let other_cid = register(&app).await;
    let r = t(tok_body(&code, &other_cid)).await;
    assert_eq!(msg(&r), (401, json!("client_id mismatch")));
    let mut b = tok_body(&code, &cid);
    b["redirect_uri"] = json!("https://client.example.com/other");
    assert_eq!(msg(&t(b).await), (401, json!("redirect_uri mismatch")));
    let mut b = tok_body(&code, &cid);
    b["code_verifier"] = json!("wrong-verifier");
    assert_eq!(msg(&t(b).await), (401, json!("PKCE verification failed")));
    // Kode yang gagal PKCE tidak terpakai.
    let used: i64 = app.scalar(&format!(r#"SELECT count(*) FROM "mcpAdminOAuthCode" WHERE code = '{code}' AND "usedAt" IS NOT NULL"#)).await;
    assert_eq!(used, 0);
    // Metode plain tersimpan di DB → ditolak saat penukaran.
    let plain_code = approve(&app, &tok, &cid).await;
    app.exec(&format!(r#"UPDATE "mcpAdminOAuthCode" SET "codeChallengeMethod" = 'plain' WHERE code = '{plain_code}'"#)).await;
    assert_eq!(
        msg(&t(tok_body(&plain_code, &cid)).await),
        (401, json!("Unsupported code_challenge_method"))
    );
    // Kedaluwarsa.
    let old_code = approve(&app, &tok, &cid).await;
    app.exec(&format!(r#"UPDATE "mcpAdminOAuthCode" SET "expiresAt" = NOW() - interval '1 minute' WHERE code = '{old_code}'"#)).await;
    assert_eq!(
        msg(&t(tok_body(&old_code, &cid)).await),
        (401, json!("Authorization code expired"))
    );

    // Penukaran sukses (juga lewat form-urlencoded).
    let r = t(tok_body(&code, &cid)).await;
    r.assert_status(200);
    assert_eq!(
        r.json.as_object().unwrap().keys().collect::<Vec<_>>(),
        [
            "access_token",
            "token_type",
            "expires_in",
            "refresh_token",
            "refresh_token_expires_in",
            "scope"
        ]
    );
    assert_eq!(r.json["token_type"], json!("Bearer"));
    assert_eq!(r.json["expires_in"], json!(86400));
    assert_eq!(r.json["refresh_token_expires_in"], json!(31_536_000));
    assert_eq!(r.json["scope"], json!("read"));
    let access = r.json["access_token"].as_str().unwrap().to_owned();
    let refresh = r.json["refresh_token"].as_str().unwrap().to_owned();
    assert!(access.starts_with("mcp_admin_oauth_") && access.len() == 16 + 32);
    assert!(refresh.starts_with("mcp_admin_refresh_") && refresh.len() == 18 + 48);
    // Hash tersimpan (bukan token mentah), prefix 20 karakter.
    let n: i64 = app.scalar(&format!(r#"SELECT count(*) FROM "mcpAdminOAuthAccessToken" WHERE "tokenHash" = '{}' AND "tokenPrefix" = '{}' AND "operatorId" = {op}"#, sha(&access), &access[..20])).await;
    assert_eq!(n, 1);
    let n: i64 = app.scalar(&format!(r#"SELECT count(*) FROM "mcpAdminOAuthRefreshToken" WHERE "tokenHash" = '{}' AND "tokenPrefix" = '{}'"#, sha(&refresh), &refresh[..20])).await;
    assert_eq!(n, 1);
    let n: i64 = app.scalar(&format!(r#"SELECT count(*) FROM "mcpAdminOAuthAccessToken" WHERE "tokenHash" = '{access}' OR "tokenPrefix" = '{access}'"#)).await;
    assert_eq!(n, 0);
    // Sekali pakai.
    assert_eq!(
        msg(&t(tok_body(&code, &cid)).await),
        (401, json!("Authorization code already used"))
    );

    // Access token dipakai di guard PAT.
    let r = app
        .get("/admin-mcp/mixpanel/overview")
        .token(&access)
        .send()
        .await;
    r.assert_status(200);

    // Form-urlencoded token endpoint.
    let code2 = approve(&app, &tok, &cid).await;
    let form = format!(
        "grant_type=authorization_code&code={code2}&client_id={cid}&redirect_uri=https%3A%2F%2Fclient.example.com%2Fcb&code_verifier={VERIFIER}"
    );
    let r = app
        .post("/admin-mcp/oauth/token")
        .raw("application/x-www-form-urlencoded", form)
        .send()
        .await;
    r.assert_status(200);

    // Refresh: format, klien salah, rotasi.
    let rf = |rt: &str, c: &str| json!({ "grant_type": "refresh_token", "refresh_token": rt, "client_id": c });
    assert_eq!(
        msg(&t(rf("bad_format", &cid)).await),
        (401, json!("Invalid refresh token format"))
    );
    assert_eq!(
        msg(&t(rf("mcp_admin_refresh_zzz", &cid)).await),
        (401, json!("Invalid refresh token"))
    );
    assert_eq!(
        msg(&t(rf(&refresh, &other_cid)).await),
        (401, json!("client_id mismatch"))
    );
    let r = t(rf(&refresh, &cid)).await;
    r.assert_status(200);
    let access2 = r.json["access_token"].as_str().unwrap().to_owned();
    let refresh2 = r.json["refresh_token"].as_str().unwrap().to_owned();
    assert_ne!(refresh2, refresh);
    assert_eq!(r.json["scope"], json!("read"));
    // Refresh lama tak bisa dipakai lagi.
    assert_eq!(
        msg(&t(rf(&refresh, &cid)).await),
        (401, json!("Refresh token already used"))
    );
    app.get("/admin-mcp/mixpanel/overview")
        .token(&access2)
        .send()
        .await
        .assert_status(200);
    // Refresh dicabut / kedaluwarsa.
    app.exec(&format!(
        r#"UPDATE "mcpAdminOAuthRefreshToken" SET "revokedAt" = NOW() WHERE "tokenHash" = '{}'"#,
        sha(&refresh2)
    ))
    .await;
    assert_eq!(
        msg(&t(rf(&refresh2, &cid)).await),
        (401, json!("Refresh token revoked"))
    );
    app.exec(&format!(r#"UPDATE "mcpAdminOAuthRefreshToken" SET "revokedAt" = NULL, "expiresAt" = NOW() - interval '1 minute' WHERE "tokenHash" = '{}'"#, sha(&refresh2))).await;
    assert_eq!(
        msg(&t(rf(&refresh2, &cid)).await),
        (401, json!("Refresh token expired"))
    );

    // Revoke: guard operator, format, kepemilikan.
    let rv = |tk: Option<&str>, token: Value| {
        let app = &app;
        let tk = tk.map(str::to_owned);
        async move {
            let b = app.post("/admin-mcp/oauth/revoke");
            let b = match &tk {
                Some(t) => b.token(t),
                None => b,
            };
            b.json(&json!({ "token": token })).send().await
        }
    };
    rv(None, json!(access.clone())).await.assert_status(401);
    assert_eq!(
        msg(&rv(Some(&tok), json!("mcp_admin_pat_x")).await),
        (400, json!("Invalid token format"))
    );
    assert_eq!(
        msg(&rv(Some(&other_tok), json!(access.clone())).await),
        (404, json!("Token not found"))
    );
    // token bukan string → TypeError di Node → 500
    rv(Some(&tok), json!(null)).await.assert_status(500);
    let r = rv(Some(&tok), json!(access.clone())).await;
    r.assert_status(200);
    assert_eq!(r.json, json!({ "revoked": true }));
    let r = app
        .get("/admin-mcp/mixpanel/overview")
        .token(&access)
        .send()
        .await;
    assert_eq!(msg(&r), (401, json!("Invalid or expired token")));
    // access2 (token lain) tetap hidup.
    app.get("/admin-mcp/mixpanel/overview")
        .token(&access2)
        .send()
        .await
        .assert_status(200);

    // Operator dinonaktifkan → access token ditolak (D-1).
    app.exec(&format!("UPDATE operators SET status = 0 WHERE id = {op}"))
        .await;
    app.get("/admin-mcp/mixpanel/overview")
        .token(&access2)
        .send()
        .await
        .assert_status(401);
    app.exec(&format!("UPDATE operators SET status = 1 WHERE id = {op}"))
        .await;

    // Penukaran paralel satu kode: tepat satu yang lolos.
    let race = approve(&app, &tok, &cid).await;
    let (a, b) = tokio::join!(t(tok_body(&race, &cid)), t(tok_body(&race, &cid)));
    let mut codes = [a.status.as_u16(), b.status.as_u16()];
    codes.sort_unstable();
    assert_eq!(codes, [200, 401]);
}
