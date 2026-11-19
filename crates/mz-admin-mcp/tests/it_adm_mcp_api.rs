//! Uji integrasi `/admin/mixpanel/*`, `/admin-mcp/mixpanel/*`, `/admin/mcp-tokens` terhadap Postgres
//! nyata. Sync Mixpanel diuji lewat server tiruan NDJSON lokal (tidak pernah menghubungi Mixpanel).
//! Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-admin-mcp --test it_adm_mcp_api`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::useless_format
)]

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use mz_core::AppState;
use mz_testkit::TestApp;
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;

static SERIAL: AsyncMutex<()> = AsyncMutex::const_new(());

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
    (
        id,
        app.token_with(json!({ "sub": id, "username": format!("op{u}") })),
    )
}

const COLS: &str = r#""date","eventName","distinctId","utmSource","utmCampaign","pageName","formName","ctaName","hour","currentDomain""#;

async fn ev(
    app: &TestApp,
    date: &str,
    event: &str,
    id: &str,
    source: Option<&str>,
    campaign: Option<&str>,
    page: Option<&str>,
    form: Option<&str>,
    hour: i32,
    domain: Option<&str>,
) {
    let q = |o: Option<&str>| o.map_or("NULL".to_owned(), |v| format!("'{v}'"));
    app.exec(&format!(
        r#"INSERT INTO "mixpanelEvent" ({COLS}) VALUES ('{date}','{event}','{id}',{},{},{},{},{},{hour},{})"#,
        q(source),
        q(campaign),
        q(page),
        q(form),
        q(Some("hero")),
        q(domain)
    ))
    .await;
}

async fn seed(app: &TestApp) {
    app.exec(r#"DELETE FROM "mixpanelEvent" WHERE "date" LIKE '2001-%'"#)
        .await;
    let pv = "Page Viewed";
    ev(
        app,
        "2001-04-02",
        pv,
        "u1",
        Some("google"),
        None,
        Some("Home"),
        None,
        10,
        Some("timemoz.com"),
    )
    .await;
    ev(
        app,
        "2001-04-02",
        pv,
        "u2",
        Some("google"),
        None,
        Some("Pricing"),
        None,
        10,
        Some("timemoz.com"),
    )
    .await;
    // dashboard (dikecualikan)
    ev(
        app,
        "2001-04-03",
        pv,
        "u1",
        None,
        None,
        Some("Sign Up"),
        None,
        11,
        Some("app.timemoz.com"),
    )
    .await;
    ev(
        app,
        "2001-04-03",
        pv,
        "u3",
        None,
        None,
        Some("Sign Up"),
        None,
        23,
        Some("timemoz.com"),
    )
    .await;
    // sebelum range (returning)
    ev(
        app,
        "2001-03-20",
        pv,
        "u1",
        None,
        None,
        Some("Home"),
        None,
        9,
        Some("timemoz.com"),
    )
    .await;
    ev(
        app,
        "2001-04-02",
        "CTA Clicked",
        "u1",
        Some("google"),
        None,
        Some("Home"),
        None,
        10,
        Some("timemoz.com"),
    )
    .await;
    ev(
        app,
        "2001-04-03",
        "Form Submitted Succeeded",
        "u3",
        None,
        Some("spring"),
        Some("Sign Up"),
        Some("Register"),
        23,
        Some("timemoz.com"),
    )
    .await;
    ev(
        app,
        "2001-04-03",
        "Form Submitted Succeeded",
        "u9",
        None,
        Some("spring"),
        None,
        Some("Other"),
        23,
        Some("timemoz.com"),
    )
    .await;
}

const RANGE: &str = "from=2001-04-01&to=2001-04-07";

fn mixpanel_paths() -> Vec<&'static str> {
    vec![
        "overview",
        "traffic",
        "pages",
        "cta",
        "funnel",
        "campaigns",
        "registrations",
    ]
}

#[tokio::test]
async fn analytics_output_and_auth() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    seed(&app).await;
    let (_, tok) = operator(&app, 1).await;
    let (_, inactive) = operator(&app, 0).await;
    let c = app.company().await;

    // Auth operator: 401 polos tanpa token / token user / operator nonaktif.
    for p in mixpanel_paths() {
        let path = format!("/admin/mixpanel/{p}?{RANGE}");
        let r = app.get(&path).send().await;
        r.assert_status(401);
        assert_eq!(
            r.json,
            json!({ "message": "Unauthorized", "statusCode": 401 })
        );
        app.get(&path)
            .user(c.admin.id)
            .send()
            .await
            .assert_status(401);
        app.get(&path)
            .token(&inactive)
            .send()
            .await
            .assert_status(401);
    }

    let get = |path: String| {
        let app = &app;
        let tok = tok.clone();
        async move { app.get(&path).token(&tok).send().await }
    };

    let r = get(format!("/admin/mixpanel/overview?{RANGE}")).await;
    r.assert_status(200);
    assert_eq!(
        r.json,
        json!({
            "success": true,
            "meta": { "from": "2001-04-01", "to": "2001-04-07" },
            "data": {
                "total_page_views": 3, "unique_visitors": 3, "total_cta_clicks": 1,
                "total_registrations": 1, "conversion_rate": 33.33, "cta_click_rate": 33.33
            }
        })
    );
    // Urutan kunci & angka bulat tanpa ".0".
    let text = r.text();
    assert!(text.starts_with(r#"{"success":true,"meta":{"from":"2001-04-01","to":"2001-04-07"},"data":{"total_page_views":3,"#), "{text}");

    // Event ber-`currentDomain` NULL TIDAK dihitung (Prisma `NOT { currentDomain: … }` =
    // SQL `NOT (col = $1)`, NULL tersaring; paritas adm-mcp 2026-11-17).
    ev(
        &app,
        "2001-04-04",
        "Page Viewed",
        "u_null",
        Some("google"),
        None,
        Some("Home"),
        None,
        9,
        None,
    )
    .await;
    let r = get(format!("/admin/mixpanel/overview?{RANGE}")).await;
    assert_eq!(r.json["data"]["total_page_views"], json!(3));
    assert_eq!(r.json["data"]["unique_visitors"], json!(3));
    app.exec(r#"DELETE FROM "mixpanelEvent" WHERE "distinctId" = 'u_null'"#)
        .await;

    let r = get(format!("/admin/mixpanel/funnel?{RANGE}")).await;
    r.assert_status(200);
    assert_eq!(
        r.json["data"]["steps"][0]["conversion_from_start"],
        json!(100)
    );
    assert_eq!(r.json["data"]["steps"][1]["count"], json!(1));
    assert_eq!(
        r.json["data"]["steps"][2]["conversion_from_start"],
        json!(33.3)
    );
    assert_eq!(
        r.json["data"]["steps"][2]["conversion_from_prev"],
        json!(100)
    );
    assert_eq!(r.json["data"]["overall_conversion"], json!(33.3));
    assert!(
        r.text()
            .contains(r#""conversion_from_start":100,"conversion_from_prev":100"#)
    );

    let r = get(format!(
        "/admin/mixpanel/traffic?from=2001-04-01&to=2001-04-03"
    ))
    .await;
    r.assert_status(200);
    assert_eq!(
        r.json["data"]["series"],
        json!([
            { "date": "2001-04-01", "value": 0 },
            { "date": "2001-04-02", "value": 2 },
            { "date": "2001-04-03", "value": 1 },
        ])
    );
    assert_eq!(
        r.json["meta"],
        json!({ "from": "2001-04-01", "to": "2001-04-03", "unit": "day" })
    );
    assert_eq!(
        r.json["data"]["by_source"],
        json!([
            { "label": "google", "value": 2, "percent": 66.7 },
            { "label": "(none)", "value": 1, "percent": 33.3 },
        ])
    );
    assert_eq!(r.json["data"]["summary"], json!({ "total": 3 }));

    let r = get(format!("/admin/mixpanel/traffic?{RANGE}&unit=week")).await;
    assert_eq!(
        r.json["data"]["series"],
        json!([{ "date": "2001-03-26", "value": 0 }, { "date": "2001-04-02", "value": 3 }])
    );

    let r = get(format!("/admin/mixpanel/campaigns?{RANGE}")).await;
    assert_eq!(
        r.json["data"]["campaigns"],
        json!([
            { "campaign": "(none)", "page_views": 3, "cta_clicks": 1, "registrations": 0, "cta_rate": 33.3, "conversion_rate": 0 },
            { "campaign": "spring", "page_views": 0, "cta_clicks": 0, "registrations": 1, "cta_rate": 0, "conversion_rate": 0 },
        ])
    );

    let r = get(format!("/admin/mixpanel/registrations?{RANGE}")).await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["summary"], json!({ "total": 1 }));
    assert_eq!(
        r.json["data"]["by_campaign"],
        json!([{ "label": "spring", "value": 1, "percent": 100 }])
    );

    // Default range (30 hari terakhir) + unit sembarang: SQL sah, bentuk benar.
    for p in mixpanel_paths() {
        let r = get(format!("/admin/mixpanel/{p}")).await;
        r.assert_status(200).assert_no_sql_error();
        assert_eq!(r.json["success"], json!(true));
    }
    get("/admin/mixpanel/traffic?unit=bogus".into())
        .await
        .assert_status(200);
    // Kunci berulang (array) tidak valid untuk `string`.
    get("/admin/mixpanel/overview?from=a&from=b".into())
        .await
        .assert_status(400);
}

#[tokio::test]
async fn mcp_pat_flow_and_extra_endpoints() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    seed(&app).await;
    let (op, tok) = operator(&app, 1).await;
    let (other_op, other_tok) = operator(&app, 1).await;

    // --- tokens ---
    app.post("/admin/mcp-tokens")
        .json(&json!({ "name": "x" }))
        .send()
        .await
        .assert_status(401);
    app.get("/admin/mcp-tokens").send().await.assert_status(401);
    app.delete(&format!("/admin/mcp-tokens/{}", uuid::Uuid::new_v4()))
        .send()
        .await
        .assert_status(401);

    let r = app
        .post("/admin/mcp-tokens")
        .token(&tok)
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
    app.post("/admin/mcp-tokens")
        .token(&tok)
        .json(&json!({ "name": "a".repeat(256) }))
        .send()
        .await
        .assert_status(400);

    let r = app
        .post("/admin/mcp-tokens")
        .token(&tok)
        .json(&json!({ "name": "My Claude", "extra": 1 }))
        .send()
        .await;
    r.assert_status(201);
    let pat = r.json["token"].as_str().unwrap().to_owned();
    let uuid = r.json["uuid"].as_str().unwrap().to_owned();
    assert!(pat.starts_with("mcp_admin_pat_") && pat.len() == 46);
    assert_eq!(r.json["prefix"], json!(&pat[..16]));
    assert_eq!(r.json["name"], json!("My Claude"));
    assert_eq!(
        r.json.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["token", "uuid", "name", "prefix"]
    );
    // Hash tersimpan, bukan token mentah.
    let hash = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(pat.as_bytes()))
    };
    let stored: i64 = app
        .scalar(&format!(r#"SELECT count(*) FROM "mcpAdminAccessToken" WHERE "tokenHash" = '{hash}' AND "operatorId" = {op}"#))
        .await;
    assert_eq!(stored, 1);

    let r = app.get("/admin/mcp-tokens").token(&tok).send().await;
    r.assert_status(200);
    let item = &r.json["data"][0];
    assert_eq!(item["uuid"], json!(uuid));
    assert_eq!(
        item.as_object().unwrap().keys().collect::<Vec<_>>(),
        [
            "uuid",
            "name",
            "tokenPrefix",
            "scopes",
            "lastUsedAt",
            "expiresAt",
            "revokedAt",
            "createdAt"
        ]
    );
    assert_eq!(item["scopes"], json!(["read"]));
    assert!(item["createdAt"].as_str().unwrap().ends_with('Z'));
    // Token operator lain tidak terlihat.
    let r = app.get("/admin/mcp-tokens").token(&other_tok).send().await;
    assert_eq!(r.json["data"], json!([]));

    // --- guard PAT ---
    let mcp = |p: &str| format!("/admin-mcp/mixpanel/{p}?{RANGE}");
    let r = app.get(&mcp("overview")).send().await;
    r.assert_status(401);
    assert_eq!(r.json["message"], json!("Missing Bearer token"));
    assert!(r.headers["www-authenticate"].to_str().unwrap().starts_with("Bearer resource_metadata=\"http://localhost:3000/admin-mcp/.well-known/oauth-protected-resource\""));
    app.get(&mcp("overview"))
        .header("authorization", "Basic abc")
        .send()
        .await
        .assert_status(401);
    app.get(&mcp("overview"))
        .header("authorization", "Bearer  mcp_admin_pat_x")
        .send()
        .await
        .assert_status(401);
    let r = app
        .get(&mcp("overview"))
        .header("authorization", "Bearer something")
        .send()
        .await;
    assert_eq!(r.json["message"], json!("Invalid token type"));
    let r = app
        .get(&mcp("overview"))
        .token("mcp_admin_pat_deadbeef")
        .send()
        .await;
    assert_eq!(
        (r.status.as_u16(), r.json["message"].clone()),
        (401, json!("Invalid or expired token"))
    );
    // JWT operator tidak diterima di guard PAT.
    app.get(&mcp("overview"))
        .token(&tok)
        .send()
        .await
        .assert_status(401);

    let r = app.get(&mcp("overview")).token(&pat).send().await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["total_page_views"], json!(3));
    // lastUsedAt diisi di latar.
    let mut used = false;
    for _ in 0..30 {
        let n: i64 = app.scalar(&format!(r#"SELECT count(*) FROM "mcpAdminAccessToken" WHERE uuid = '{uuid}' AND "lastUsedAt" IS NOT NULL"#)).await;
        if n == 1 {
            used = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(used, "lastUsedAt tidak terisi");

    // 13 route PAT: SQL sah + bentuk.
    for p in [
        "overview",
        "traffic",
        "pages",
        "cta",
        "funnel",
        "campaigns",
        "registrations",
        "geo",
        "devices",
        "hourly",
        "day-of-week",
        "new-vs-returning",
        "comparison",
    ] {
        let r = app.get(&mcp(p)).token(&pat).send().await;
        r.assert_status(200).assert_no_sql_error();
        assert_eq!(r.json["success"], json!(true), "{p}");
        // tanpa query → rentang bawaan
        let r = app
            .get(&format!("/admin-mcp/mixpanel/{p}"))
            .token(&pat)
            .send()
            .await;
        r.assert_status(200).assert_no_sql_error();
    }

    let r = app.get(&mcp("hourly")).token(&pat).send().await;
    assert_eq!(r.json["data"]["series"].as_array().unwrap().len(), 24);
    assert_eq!(
        r.json["data"]["series"][10],
        json!({ "hour": 10, "label": "10:00", "value": 2 })
    );
    assert_eq!(
        r.json["data"]["series"][23],
        json!({ "hour": 23, "label": "23:00", "value": 1 })
    );
    assert_eq!(
        r.json["data"]["peak_hour"],
        json!({ "hour": 10, "label": "10:00", "value": 2 })
    );

    let r = app.get(&mcp("day-of-week")).token(&pat).send().await;
    assert_eq!(
        r.json["data"]["series"][1],
        json!({ "day": 1, "label": "Monday", "value": 2 })
    );
    assert_eq!(
        r.json["data"]["series"][2],
        json!({ "day": 2, "label": "Tuesday", "value": 1 })
    );
    assert_eq!(
        r.json["data"]["peak_day"],
        json!({ "day": 1, "label": "Monday", "value": 2 })
    );

    let r = app.get(&mcp("new-vs-returning")).token(&pat).send().await;
    assert_eq!(
        r.json["data"],
        json!({ "new_visitors": 2, "returning_visitors": 1, "total": 3, "returning_rate": 33.3 })
    );

    let r = app.get(&mcp("geo")).token(&pat).send().await;
    assert_eq!(
        r.json["data"]["by_country"],
        json!([{ "label": "(none)", "value": 3, "percent": 100 }])
    );

    let r = app.get(&mcp("comparison")).token(&pat).send().await;
    assert_eq!(
        r.json["meta"],
        json!({
            "current": { "from": "2001-04-01", "to": "2001-04-07", "days": 7 },
            "previous": { "from": "2001-03-25", "to": "2001-03-31", "days": 7 },
        })
    );
    assert_eq!(r.json["data"]["previous"]["page_views"], json!(0));
    assert_eq!(
        r.json["data"]["change"],
        json!({
            "page_views": 100, "unique_visitors": 100, "cta_clicks": 100, "registrations": 100, "conversion_rate": 100
        })
    );
    // Tanggal tak valid → 500 (RangeError di Node).
    app.get("/admin-mcp/mixpanel/comparison?from=bad&to=worse")
        .token(&pat)
        .send()
        .await
        .assert_status(500);

    // --- revoke ---
    app.delete("/admin/mcp-tokens/not-a-uuid")
        .token(&tok)
        .send()
        .await
        .assert_status(400);
    let r = app
        .delete("/admin/mcp-tokens/not-a-uuid")
        .token(&tok)
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!("Validation failed (uuid is expected)")
    );
    let r = app
        .delete(&format!("/admin/mcp-tokens/{uuid}"))
        .token(&other_tok)
        .send()
        .await;
    assert_eq!(
        (r.status.as_u16(), r.json["message"].clone()),
        (404, json!("Token not found"))
    );
    let _ = other_op;
    let r = app
        .delete(&format!("/admin/mcp-tokens/{uuid}"))
        .token(&tok)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json, json!({ "success": true }));
    // Dicabut → ditolak, dan tak bisa dicabut dua kali.
    app.get(&mcp("overview"))
        .token(&pat)
        .send()
        .await
        .assert_status(401);
    app.delete(&format!("/admin/mcp-tokens/{uuid}"))
        .token(&tok)
        .send()
        .await
        .assert_status(404);

    // Operator nonaktif / token kedaluwarsa ditolak.
    let r = app
        .post("/admin/mcp-tokens")
        .token(&tok)
        .json(&json!({ "name": "t2" }))
        .send()
        .await;
    let pat2 = r.json["token"].as_str().unwrap().to_owned();
    app.get(&mcp("overview"))
        .token(&pat2)
        .send()
        .await
        .assert_status(200);
    app.exec(&format!(r#"UPDATE "mcpAdminAccessToken" SET "expiresAt" = NOW() - interval '1 hour' WHERE "operatorId" = {op}"#)).await;
    app.get(&mcp("overview"))
        .token(&pat2)
        .send()
        .await
        .assert_status(401);
    app.exec(&format!(
        r#"UPDATE "mcpAdminAccessToken" SET "expiresAt" = NULL WHERE "operatorId" = {op}"#
    ))
    .await;
    app.get(&mcp("overview"))
        .token(&pat2)
        .send()
        .await
        .assert_status(200);
    app.exec(&format!("UPDATE operators SET status = 0 WHERE id = {op}"))
        .await;
    app.get(&mcp("overview"))
        .token(&pat2)
        .send()
        .await
        .assert_status(401);
    app.exec(&format!("UPDATE operators SET status = 1 WHERE id = {op}"))
        .await;
}

// ───── sync dengan server Mixpanel tiruan ─────

#[derive(Clone, Default)]
struct Mock {
    log: Arc<Mutex<Vec<(Vec<(String, String)>, String)>>>,
    fail_event: Arc<Mutex<Option<String>>>,
}

async fn export(
    State(m): State<Mock>,
    Query(q): Query<Vec<(String, String)>>,
    headers: HeaderMap,
) -> (StatusCode, String) {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let event: String = q
        .iter()
        .find(|(k, _)| k == "event")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    m.log.lock().unwrap().push((q.clone(), auth));
    if m.fail_event
        .lock()
        .unwrap()
        .as_deref()
        .is_some_and(|f| event.contains(f))
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, "boom".into());
    }
    let date = q
        .iter()
        .find(|(k, _)| k == "from_date")
        .map(|(_, v)| v.as_str())
        .unwrap_or_default();
    let ts = 1_700_000_000_i64; // 22:13:20 UTC
    let body = if event.contains("Page Viewed") {
        [
            json!({"event":"Page Viewed","properties":{"distinct_id":"m1","time":ts,"$current_url":"https://timemoz.com/pricing","utm_source":"g","page_name":"Pricing","$country_code":"ID","path":"/pricing"}}),
            json!({"event":"Page Viewed","properties":{"distinct_id":"m2","time":ts,"$current_url":"https://app.timemoz.com/x"}}),
            json!({"event":"Page Viewed","properties":{"distinct_id":"m3","time":ts}}),
            json!({"event":"Page Viewed","properties":{"distinct_id":"m4","$current_url":"https://other.com/"}}),
        ]
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
            + "\n\n"
    } else {
        json!({"event": event, "properties": {"distinct_id": "z1", "form_name": "Register", "time": ts, "$current_url": "https://timemoz.com/", "d": date}}).to_string() + "\r\n"
    };
    (StatusCode::OK, body)
}

async fn start_mock() -> (String, Mock) {
    let mock = Mock::default();
    let app = Router::new()
        .route("/export", get(export))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });
    (url, mock)
}

#[tokio::test]
async fn sync_with_mock_mixpanel_and_audit() {
    let _g = SERIAL.lock().await;
    let (base, mock) = start_mock().await;
    let Some(app) = TestApp::with_vars(
        build,
        &[
            ("MZ_MIXPANEL_EXPORT_BASE", &base),
            ("MIXPANEL_USERNAME", "svc-user"),
            ("MIXPANEL_SECRET_KEY", "svc-secret"),
            ("MIXPANEL_PROJECT_ID", "424242"),
        ],
    )
    .await
    else {
        return;
    };
    let (op, tok) = operator(&app, 1).await;
    app.post("/admin/mixpanel/sync")
        .send()
        .await
        .assert_status(401);

    let date = (chrono::Utc::now().date_naive() - chrono::Duration::days(10))
        .format("%Y-%m-%d")
        .to_string();
    app.exec(&format!(
        r#"DELETE FROM "mixpanelEvent" WHERE "date" = '{date}'"#
    ))
    .await;
    // Baris lama di tanggal sama dengan event yang akan di-sync → harus terhapus (delete + insert).
    app.exec(&format!(
        r#"INSERT INTO "mixpanelEvent" ("date","eventName","distinctId") VALUES ('{date}','Page Viewed','stale')"#
    ))
    .await;

    let r = app
        .post(&format!("/admin/mixpanel/sync?from={date}&to={date}"))
        .token(&tok)
        .json(&json!({ "reason": "backfill", "password": "x" }))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(
        r.json,
        json!({ "success": true, "message": format!("Sync completed for range {date} – {date}") })
    );

    // Request ke Mixpanel: 3 event, kredensial Basic, project_id, tanggal.
    let log = mock.log.lock().unwrap().clone();
    assert_eq!(log.len(), 3);
    let (q, auth) = &log[0];
    assert_eq!(auth, "Basic c3ZjLXVzZXI6c3ZjLXNlY3JldA==");
    let get = |k: &str| {
        q.iter()
            .find(|(kk, _)| kk == k)
            .map(|(_, v)| v.clone())
            .unwrap()
    };
    assert_eq!(get("project_id"), "424242");
    assert_eq!(get("from_date"), date);
    assert_eq!(get("to_date"), date);
    assert_eq!(get("event"), r#"["Page Viewed"]"#);

    // Page Viewed: m1 (timemoz.com) + m3 (tanpa URL); m2 (dashboard) & m4 (domain lain) disaring.
    let rows: Vec<(String, Option<String>, Option<i32>, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        r#"SELECT "distinctId","currentDomain","hour","utmSource","country","path" FROM "mixpanelEvent"
            WHERE "date" = $1 AND "eventName" = 'Page Viewed' ORDER BY "distinctId""#,
    )
    .bind(&date)
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            (
                "m1".into(),
                Some("timemoz.com".into()),
                Some(22),
                Some("g".into()),
                Some("ID".into()),
                Some("/pricing".into())
            ),
            ("m3".into(), None, Some(22), None, None, None),
        ]
    );
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "mixpanelEvent" WHERE "date" = '{date}'"#
        ))
        .await;
    assert_eq!(n, 4); // 2 Page Viewed + CTA + Form
    // Hasil sync tampak di query analitik.
    let r = app
        .get(&format!("/admin/mixpanel/overview?from={date}&to={date}"))
        .token(&tok)
        .send()
        .await;
    // Hanya m1: m3 (tanpa URL → `currentDomain` NULL) tidak dihitung, sama dengan Prisma
    // `NOT { currentDomain: 'app.timemoz.com' }` (paritas adm-mcp 2026-11-17).
    assert_eq!(r.json["data"]["total_page_views"], json!(1));
    assert_eq!(r.json["data"]["total_registrations"], json!(1));

    // Audit.
    let audit: Vec<(String, String, Option<Value>)> = sqlx::query_as(
        r#"SELECT action, "targetType", after FROM "operatorAuditLog" WHERE "operatorId" = $1 ORDER BY id"#,
    )
    .bind(op)
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].0, "mixpanel.sync");
    assert_eq!(audit[0].1, "system");
    assert_eq!(audit[0].2, Some(json!({ "reason": "backfill" })));

    // Tanpa parameter: sync kemarin.
    mock.log.lock().unwrap().clear();
    let yesterday = (chrono::Utc::now().date_naive() - chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();
    let r = app.post("/admin/mixpanel/sync").token(&tok).send().await;
    r.assert_status(201);
    assert_eq!(
        r.json["message"],
        json!(format!("Sync completed for {yesterday}"))
    );
    assert_eq!(mock.log.lock().unwrap().len(), 3);
    // from saja (tanpa to) → juga kemarin; from/to di body diterima.
    let r = app
        .post(&format!("/admin/mixpanel/sync?from={date}"))
        .token(&tok)
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!(format!("Sync completed for {yesterday}"))
    );
    mock.log.lock().unwrap().clear();
    let r = app
        .post("/admin/mixpanel/sync")
        .token(&tok)
        .json(&json!({ "from": date, "to": date }))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!(format!("Sync completed for range {date} – {date}"))
    );
    assert_eq!(mock.log.lock().unwrap().len(), 3);

    // Satu event gagal: yang lain tetap tersimpan, respons 500, TIDAK ada baris audit baru.
    *mock.fail_event.lock().unwrap() = Some("CTA Clicked".into());
    app.exec(&format!(
        r#"DELETE FROM "mixpanelEvent" WHERE "date" = '{date}'"#
    ))
    .await;
    let before: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "operatorAuditLog" WHERE "operatorId" = {op}"#
        ))
        .await;
    let r = app
        .post(&format!("/admin/mixpanel/sync?from={date}&to={date}"))
        .token(&tok)
        .send()
        .await;
    r.assert_status(500);
    assert_eq!(
        r.json,
        json!({ "statusCode": 500, "message": "Internal server error" })
    );
    let n: i64 = app.scalar(&format!(r#"SELECT count(*) FROM "mixpanelEvent" WHERE "date" = '{date}' AND "eventName" <> 'CTA Clicked'"#)).await;
    assert_eq!(n, 3);
    let after: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "operatorAuditLog" WHERE "operatorId" = {op}"#
        ))
        .await;
    assert_eq!(before, after);
}
