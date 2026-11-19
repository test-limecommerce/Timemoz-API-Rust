//! Uji integrasi `google-calendar/*`, `google-calendar/user/*`, webhook, dan cron terhadap
//! Postgres nyata + Google tiruan (`MockServer`). Jalankan:
//! `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-int-gcal --test it_int_gcal_api`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::Duration;

use axum::Router;
use mz_core::AppState;
use mz_int_common::mock::{MockServer, Recorded};
use mz_int_gcal::util::{QVal, parse_query, query_get};
use mz_testkit::{Company, TestApp};
use serde_json::{Value, json};
use tokio::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::const_new(());

const EVENTS: &str = "/calendar/v3/calendars/primary/events";

fn build(state: &AppState) -> Router {
    mz_int_gcal::router(state)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

async fn setup() -> Option<(TestApp, MockServer, Company)> {
    let mock = MockServer::start().await;
    let url = mock.url();
    let app = TestApp::with_vars(
        build,
        &[
            ("MZ_GOOGLE_OAUTH_BASE", &url),
            ("MZ_GOOGLE_APIS_BASE", &url),
            ("GOOGLE_CALENDAR_CLIENT_ID", "cid"),
            ("GOOGLE_CALENDAR_CLIENT_SECRET", "csecret"),
            (
                "GOOGLE_CALENDAR_REDIRECT_URI",
                "http://be.test/google-calendar/callback",
            ),
            (
                "GOOGLE_CALENDAR_USER_REDIRECT_URI",
                "http://be.test/google-calendar/user/callback",
            ),
            (
                "GOOGLE_CALENDAR_WEBHOOK_URL",
                "http://be.test/google-calendar/webhook",
            ),
            ("FRONTEND_URL", "http://fe.test"),
        ],
    )
    .await?;
    let fx = app.company().await;
    reset(&app, &fx).await;
    Some((app, mock, fx))
}

/// Pulihkan kolom Google Calendar fixture.
async fn reset(app: &TestApp, fx: &Company) {
    let users = [
        fx.super_admin.id,
        fx.admin.id,
        fx.manager.id,
        fx.staff.id,
        fx.staff2.id,
    ]
    .map(|i| i.to_string())
    .join(",");
    app.exec(&format!(
        r#"UPDATE company SET "googleCalendarAccessToken" = NULL, "googleCalendarRefreshToken" = NULL,
             "googleCalendarTokenExpiresAt" = NULL, "googleCalendarEmail" = NULL,
             "googleCalendarSyncToken" = NULL, "googleCalendarWebhookChannelId" = NULL,
             "googleCalendarWebhookResourceId" = NULL, "googleCalendarWebhookExpiry" = NULL
           WHERE id = {c};
           UPDATE users SET "googleCalendarAccessToken" = NULL, "googleCalendarRefreshToken" = NULL,
             "googleCalendarTokenExpiresAt" = NULL, "googleCalendarEmail" = NULL,
             "googleCalendarSyncToken" = NULL WHERE id IN ({users});
           DELETE FROM "googleCalendarEvent" WHERE "companyId" = {c};"#,
        c = fx.id
    ))
    .await;
}

/// Company "terhubung": token akses masih berlaku 1 jam.
async fn connect_company(app: &TestApp, fx: &Company, sync_token: Option<&str>) {
    let st = sync_token.map_or("NULL".into(), |s| format!("'{s}'"));
    app.exec(&format!(
        r#"UPDATE company SET "googleCalendarAccessToken" = 'at0', "googleCalendarRefreshToken" = 'rt0',
             "googleCalendarTokenExpiresAt" = NOW() + interval '1 hour', "googleCalendarSyncToken" = {st}
           WHERE id = {}"#,
        fx.id
    ))
    .await;
}

fn q(r: &Recorded, key: &str) -> Option<String> {
    let parsed = parse_query(Some(&r.query));
    query_get(&parsed, key)
        .and_then(QVal::as_str)
        .map(str::to_owned)
}

fn location(res: &mz_testkit::TestResponse) -> String {
    res.headers
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

async fn wait_until<F, Fut>(mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..100 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("kondisi tidak tercapai dalam 5 detik");
}

fn items() -> Value {
    json!({
        "items": [
            {"id": "ev1", "summary": "Standup", "status": "confirmed",
             "start": {"dateTime": "2026-05-20T09:00:00+07:00"},
             "end": {"dateTime": "2026-05-20T09:30:00+07:00"},
             "attendees": [{"email": "a@co.com", "displayName": "A", "responseStatus": "accepted"}]},
            {"id": "ev2", "status": "cancelled",
             "start": {"date": "2026-05-21"}, "end": {"date": "2026-05-22"}},
            {"summary": "tanpa id"}
        ],
        "nextSyncToken": "st-new"
    })
}

#[tokio::test]
async fn it_int_gcal_company_oauth_flow() {
    let _g = SERIAL.lock().await;
    let Some((app, mock, fx)) = setup().await else {
        return;
    };
    // auth-url
    let res = app
        .get("/google-calendar/auth-url")
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_status(200);
    assert!(res.text().starts_with(r#"{"success":true,"data":{"url":"#));
    let url = res.json["data"]["url"].as_str().unwrap().to_owned();
    assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?response_type=code&client_id=cid&redirect_uri=http%3A%2F%2Fbe.test%2Fgoogle-calendar%2Fcallback&scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcalendar.readonly+"));
    assert!(url.ends_with("&access_type=offline&prompt=consent"));
    let parsed = parse_query(url.split_once('?').map(|p| p.1));
    let state = query_get(&parsed, "state")
        .and_then(QVal::as_str)
        .unwrap()
        .to_owned();

    // STAFF tidak boleh (RolesGuard Node melempar 401 "Access denied: insufficient permissions").
    app.get("/google-calendar/auth-url")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(401);

    // callback sukses
    let exp = (chrono::Utc::now().timestamp_millis() + 7 * 86_400_000).to_string();
    mock.on(
        "POST",
        "/token",
        200,
        json!({"access_token": "at1", "refresh_token": "rt1", "expires_in": 3600}),
    );
    mock.on(
        "GET",
        "/oauth2/v2/userinfo",
        200,
        json!({"email": "cal@co.com"}),
    );
    mock.on("GET", EVENTS, 200, items());
    mock.on(
        "POST",
        "/calendar/v3/calendars/primary/events/watch",
        200,
        json!({"id": "ch1", "resourceId": "res1", "expiration": exp}),
    );
    let res = app
        .get(&format!(
            "/google-calendar/callback?code=c%2F1&state={state}"
        ))
        .send()
        .await;
    res.assert_status(302);
    assert_eq!(
        location(&res),
        "http://fe.test/settings/integrations?connected=google_calendar"
    );
    assert!(
        res.text()
            .starts_with("Found. Redirecting to http://fe.test")
    );
    let tok = &mock.requests_to("POST", "/token")[0];
    // axios `post(url, null)` = `Content-Length: 0`; tanpa header ini Google menjawab 411.
    assert_eq!(tok.header("content-length"), Some("0"));
    assert_eq!(q(tok, "grant_type").as_deref(), Some("authorization_code"));
    assert_eq!(q(tok, "code").as_deref(), Some("c/1"));
    assert_eq!(q(tok, "client_secret").as_deref(), Some("csecret"));
    assert_eq!(
        mock.requests_to("GET", "/oauth2/v2/userinfo")[0].header("authorization"),
        Some("Bearer at1")
    );

    // sync awal + watch berjalan di latar.
    let db = app.db().clone();
    let cid = fx.id;
    wait_until(|| {
        let db = db.clone();
        async move {
            let r: (Option<String>, Option<String>) = sqlx::query_as(
                r#"SELECT "googleCalendarSyncToken", "googleCalendarWebhookChannelId" FROM company WHERE id = $1"#,
            )
            .bind(cid)
            .fetch_one(&db)
            .await
            .unwrap();
            r == (Some("st-new".into()), Some("ch1".into()))
        }
    })
    .await;
    let watch = &mock.requests_to("POST", "/calendar/v3/calendars/primary/events/watch")[0];
    assert_eq!(watch.json()["type"], "web_hook");
    assert_eq!(
        watch.json()["address"],
        "http://be.test/google-calendar/webhook"
    );
    let list = &mock.requests_to("GET", EVENTS)[0];
    assert_eq!(q(list, "singleEvents").as_deref(), Some("true"));
    assert_eq!(q(list, "maxResults").as_deref(), Some("250"));
    assert!(q(list, "timeMin").is_some() && q(list, "orderBy").is_none());
    let n: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "googleCalendarEvent" WHERE "companyId" = {cid} AND "userId" IS NULL"#
        ))
        .await;
    assert_eq!(n, 2);

    // status
    let res = app
        .get("/google-calendar/status")
        .token(&fx.manager.token)
        .send()
        .await;
    res.assert_ok();
    assert!(
        res.text()
            .starts_with(r#"{"success":true,"message":"Success","data":{"connected":true,"email":"cal@co.com","webhookExpiry":""#)
    );
    // company.update di callback tercatat audit (route publik → pelaku system).
    wait_until(|| {
        let db = db.clone();
        async move {
            let n: i64 = sqlx::query_scalar(
                r#"SELECT COUNT(*) FROM "auditLog" WHERE "companyId" = $1
                   AND "entityType"::text = 'company' AND route LIKE '%/google-calendar/callback%'"#,
            )
            .bind(cid)
            .fetch_one(&db)
            .await
            .unwrap();
            n >= 1
        }
    })
    .await;
    // Token tidak bocor ke audit log sebagai nilai.
    let leaked: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "auditLog" WHERE "companyId" = {cid}
               AND (COALESCE("after"::text, '') LIKE '%rt1%' OR COALESCE("before"::text, '') LIKE '%rt1%')"#
        ))
        .await;
    assert_eq!(leaked, 0);

    // state rusak / tipe salah
    let res = app
        .get("/google-calendar/callback?code=x&state=bad")
        .send()
        .await;
    assert_eq!(
        location(&res),
        "http://fe.test/settings/integrations?error=google_calendar&message=Invalid%20state"
    );
    let user_state = app.state.clone();
    let mut p = serde_json::Map::new();
    p.insert("userId".into(), json!(fx.admin.id));
    p.insert("type".into(), json!("user"));
    let st = mz_int_common::nest_jwt::sign(&user_state, p, Some(600)).unwrap();
    let res = app
        .get(&format!("/google-calendar/callback?state={st}"))
        .header("accept", "text/html")
        .send()
        .await;
    assert_eq!(
        location(&res),
        "http://fe.test/settings/integrations?error=google_calendar&message=Invalid%20state%20type"
    );
    // Express 4.22.1: `<p>Found. Redirecting to <url ter-escape></p>` (tanpa tautan `<a>`).
    assert_eq!(
        res.text(),
        "<p>Found. Redirecting to http://fe.test/settings/integrations?error=google_calendar&amp;message=Invalid%20state%20type</p>"
    );

    // token Google gagal (400 invalid_grant)
    mock.on("POST", "/token", 400, json!({"error": "invalid_grant"}));
    let res = app
        .get(&format!("/google-calendar/callback?code=c&state={state}"))
        .send()
        .await;
    assert_eq!(
        location(&res),
        "http://fe.test/settings/integrations?error=google_calendar&message=Request%20failed%20with%20status%20code%20400"
    );

    // disconnect: stop channel + revoke + hapus event google
    mock.clear_log();
    mock.on("POST", "/calendar/v3/channels/stop", 204, Value::Null);
    mock.on("POST", "/revoke", 200, json!({}));
    app.exec(&format!(
        r#"UPDATE company SET "googleCalendarTokenExpiresAt" = NOW() + interval '1 hour' WHERE id = {cid};
           INSERT INTO "googleCalendarEvent" ("companyId", "calendarId", title, "startAt", "endAt", status, source)
           VALUES ({cid}, 'primary', 'lokal', NOW(), NOW() + interval '1 hour', 'confirmed', 'local')"#
    ))
    .await;
    app.delete("/google-calendar/disconnect")
        .token(&fx.manager.token)
        .send()
        .await
        .assert_status(401);
    let res = app
        .delete("/google-calendar/disconnect")
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(
        res.json,
        json!({"success": true, "message": "Google Calendar disconnected", "data": null})
    );
    let stop = &mock.requests_to("POST", "/calendar/v3/channels/stop")[0];
    assert_eq!(stop.json(), json!({"id": "ch1", "resourceId": "res1"}));
    let revoke = &mock.requests_to("POST", "/revoke")[0];
    assert_eq!(q(revoke, "token").as_deref(), Some("at1"));
    assert_eq!(revoke.header("content-length"), Some("0"));
    let left: Vec<String> =
        sqlx::query_scalar(r#"SELECT source FROM "googleCalendarEvent" WHERE "companyId" = $1"#)
            .bind(cid)
            .fetch_all(app.db())
            .await
            .unwrap();
    assert_eq!(left, vec!["local".to_owned()]);
    let still: Option<String> = app
        .scalar(&format!(
            r#"SELECT "googleCalendarRefreshToken" FROM company WHERE id = {cid}"#
        ))
        .await;
    assert!(still.is_none());
    reset(&app, &fx).await;
}

#[tokio::test]
async fn it_int_gcal_sync_refresh_and_errors() {
    let _g = SERIAL.lock().await;
    let Some((app, mock, fx)) = setup().await else {
        return;
    };
    // Belum terhubung.
    let res = app
        .post("/google-calendar/sync")
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(
        res.json,
        json!({"success": false, "message": "Google Calendar not connected", "data": null})
    );

    // 410 pada sync incremental → syncToken dihapus → sync penuh (juga 410 → gagal).
    connect_company(&app, &fx, Some("old")).await;
    mock.on("GET", EVENTS, 410, json!({"error": {"code": 410}}));
    let res = app
        .post("/google-calendar/sync")
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(
        res.json,
        json!({"success": false, "message": "Request failed with status code 410", "data": null})
    );
    let gets = mock.requests_to("GET", EVENTS);
    assert_eq!(gets.len(), 2);
    assert_eq!(q(&gets[0], "syncToken").as_deref(), Some("old"));
    assert_eq!(q(&gets[0], "showDeleted").as_deref(), Some("true"));
    assert!(q(&gets[1], "syncToken").is_none() && q(&gets[1], "timeMin").is_some());
    assert_eq!(gets[1].header("authorization"), Some("Bearer at0"));
    let st: Option<String> = app
        .scalar(&format!(
            r#"SELECT "googleCalendarSyncToken" FROM company WHERE id = {}"#,
            fx.id
        ))
        .await;
    assert!(st.is_none());

    // Sukses.
    mock.on("GET", EVENTS, 200, items());
    let res = app
        .post("/google-calendar/sync")
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(
        res.json,
        json!({"success": true, "message": "Sync complete: 1 events synced, 1 cancelled",
               "data": {"upserted": 1, "cancelled": 1}})
    );

    // Token kedaluwarsa → refresh; Google 400 invalid_grant → gagal.
    mock.clear_log();
    app.exec(&format!(
        r#"UPDATE company SET "googleCalendarTokenExpiresAt" = NOW() - interval '1 minute' WHERE id = {}"#,
        fx.id
    ))
    .await;
    mock.on("POST", "/token", 400, json!({"error": "invalid_grant"}));
    let res = app
        .post("/google-calendar/sync")
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(res.json["message"], "Request failed with status code 400");
    let tok = &mock.requests_to("POST", "/token")[0];
    assert_eq!(tok.header("content-length"), Some("0"));
    assert_eq!(q(tok, "grant_type").as_deref(), Some("refresh_token"));
    assert_eq!(q(tok, "refresh_token").as_deref(), Some("rt0"));

    // Refresh sukses → token baru dipakai dan disimpan.
    mock.clear_log();
    mock.on(
        "POST",
        "/token",
        200,
        json!({"access_token": "at-new", "expires_in": 3599}),
    );
    let res = app
        .post("/google-calendar/sync")
        .token(&fx.admin.token)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(
        mock.requests_to("GET", EVENTS)[0].header("authorization"),
        Some("Bearer at-new")
    );
    let at: Option<String> = app
        .scalar(&format!(
            r#"SELECT "googleCalendarAccessToken" FROM company WHERE id = {}"#,
            fx.id
        ))
        .await;
    assert_eq!(at.as_deref(), Some("at-new"));

    // calendars: sukses, 401, 5xx
    mock.on(
        "GET",
        "/calendar/v3/users/me/calendarList",
        200,
        json!({"items": [{"id": "primary", "summary": "Kantor", "accessRole": "owner"},
                         {"id": "x", "primary": true, "backgroundColor": "blue"}]}),
    );
    let res = app
        .get("/google-calendar/calendars")
        .token(&fx.manager.token)
        .send()
        .await;
    res.assert_ok();
    assert!(res.text().contains(
        r#"[{"id":"primary","summary":"Kantor","description":null,"primary":false,"accessRole":"owner","backgroundColor":null},{"id":"x","description":null,"primary":true,"backgroundColor":"blue"}]"#
    ));
    for (status, msg) in [
        (401, "Request failed with status code 401"),
        (503, "Request failed with status code 503"),
    ] {
        mock.on(
            "GET",
            "/calendar/v3/users/me/calendarList",
            status,
            json!({"error": "x"}),
        );
        let res = app
            .get("/google-calendar/calendars")
            .token(&fx.manager.token)
            .send()
            .await;
        res.assert_status(200);
        assert_eq!(
            res.json,
            json!({"success": false, "message": msg, "data": null})
        );
    }

    // Webhook: handshake `sync` diabaikan, channel dikenal → sync.
    app.exec(&format!(
        r#"UPDATE company SET "googleCalendarWebhookChannelId" = 'ch-w' WHERE id = {}"#,
        fx.id
    ))
    .await;
    mock.clear_log();
    let res = app
        .post("/google-calendar/webhook")
        .header("x-goog-channel-id", "ch-w")
        .header("x-goog-resource-state", "sync")
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.text(), r#"{"ok":true}"#);
    assert!(mock.requests_to("GET", EVENTS).is_empty());
    app.post("/google-calendar/webhook")
        .header("x-goog-channel-id", "ch-w")
        .header("x-goog-resource-state", "exists")
        .send()
        .await
        .assert_status(200);
    assert_eq!(mock.requests_to("GET", EVENTS).len(), 1);
    // channel tak dikenal / Google 500 → tetap {ok:true}
    mock.on("GET", EVENTS, 500, json!({}));
    let res = app
        .post("/google-calendar/webhook")
        .header("x-goog-channel-id", "ch-w")
        .send()
        .await;
    assert_eq!(res.text(), r#"{"ok":true}"#);
    reset(&app, &fx).await;
}

#[tokio::test]
async fn it_int_gcal_company_events() {
    let _g = SERIAL.lock().await;
    let Some((app, mock, fx)) = setup().await else {
        return;
    };
    let body = json!({"title": "Sprint", "startAt": "2026-05-20T09:00:00+07:00",
                      "endAt": "2026-05-20T10:00:00+07:00", "attendeeUserIds": [fx.staff.id, fx.staff.id]});
    // Validasi.
    let res = app
        .post("/google-calendar/events")
        .token(&fx.admin.token)
        .json(&json!({"startAt": "x"}))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "title must be shorter than or equal to 500 characters",
            "title should not be empty",
            "title must be a string",
            "startAt must be a valid ISO 8601 date string",
            "endAt must be a valid ISO 8601 date string"
        ])
    );
    let mut bad = body.clone();
    bad["endAt"] = json!("2026-05-20T08:00:00+07:00");
    let res = app
        .post("/google-calendar/events")
        .token(&fx.admin.token)
        .json(&bad)
        .send()
        .await;
    res.assert_status(201);
    assert_eq!(res.json["message"], "endAt must be after startAt");
    let mut bad = body.clone();
    bad["attendeeUserIds"] = json!([fx.staff.id, 999_999_999]);
    let res = app
        .post("/google-calendar/events")
        .token(&fx.admin.token)
        .json(&bad)
        .send()
        .await;
    assert_eq!(
        res.json["message"],
        "Invalid attendee: one or more users not found in this company"
    );

    // Tidak terhubung → lokal (undangan email jalan di latar; tanpa MAILTRAP_TOKEN = no-op).
    let res = app
        .post("/google-calendar/events")
        .token(&fx.admin.token)
        .json(&body)
        .send()
        .await;
    res.assert_status(201);
    res.assert_ok();
    assert_eq!(
        res.json["message"],
        "Event created locally (Google Calendar not connected for this company)"
    );
    let d = res.data();
    assert_eq!(d["origin"], "local");
    assert_eq!(d["googleEventId"], Value::Null);
    assert_eq!(d["startAt"], "2026-05-20T02:00:00.000Z");
    assert_eq!(d["createdBy"]["id"], fx.admin.id);
    assert_eq!(d["attendees"].as_array().unwrap().len(), 1);
    assert_eq!(d["attendees"][0]["userId"], fx.staff.id);
    let local_uuid = d["id"].as_str().unwrap().to_owned();
    assert!(mock.requests().is_empty());

    // Terhubung → Google.
    connect_company(&app, &fx, None).await;
    mock.on(
        "POST",
        EVENTS,
        200,
        json!({"id": "g1", "htmlLink": "https://cal/g1", "status": "confirmed"}),
    );
    let mut all_day = body.clone();
    all_day["isAllDay"] = json!(true);
    all_day["description"] = json!("Agenda");
    let res = app
        .post("/google-calendar/events")
        .token(&fx.manager.token)
        .json(&all_day)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(
        res.json["message"],
        "Event created and synced to Google Calendar"
    );
    assert_eq!(res.data()["googleEventId"], "g1");
    assert_eq!(res.data()["htmlLink"], "https://cal/g1");
    let ins = &mock.requests_to("POST", EVENTS)[0];
    assert_eq!(q(ins, "sendUpdates").as_deref(), Some("all"));
    assert_eq!(ins.header("authorization"), Some("Bearer at0"));
    let staff_email: String = app
        .scalar(&format!(
            "SELECT username FROM users WHERE id = {}",
            fx.staff.id
        ))
        .await;
    assert_eq!(
        ins.json(),
        json!({"summary": "Sprint", "start": {"date": "2026-05-20"}, "end": {"date": "2026-05-20"},
               "description": "Agenda", "attendees": [{"email": staff_email}]})
    );
    let g_uuid = res.data()["id"].as_str().unwrap().to_owned();

    // 403 → jatuh ke lokal; 500 → gagal.
    mock.on("POST", EVENTS, 403, json!({"error": "forbidden"}));
    let res = app
        .post("/google-calendar/events")
        .token(&fx.admin.token)
        .json(&json!({"title": "X", "startAt": "2026-05-20", "endAt": "2026-05-21"}))
        .send()
        .await;
    assert_eq!(res.data()["origin"], "local");
    mock.on("POST", EVENTS, 500, json!({}));
    let res = app
        .post("/google-calendar/events")
        .token(&fx.admin.token)
        .json(&json!({"title": "X", "startAt": "2026-05-20", "endAt": "2026-05-21"}))
        .send()
        .await;
    assert_eq!(
        res.json,
        json!({"success": false, "message": "Request failed with status code 500", "data": null})
    );

    // Daftar event + kalender bulanan.
    let res = app
        .get("/google-calendar/events?startDate=2026-05-01T00:00:00Z&endDate=2026-05-31T23:59:59Z&take=2")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.json["meta"], json!({"total": 3, "skip": 0, "take": 2}));
    assert_eq!(res.data().as_array().unwrap().len(), 2);
    assert!(res.text().contains(r#""source":"company","origin":"#));
    let res = app
        .get("/google-calendar/events?startDate=nope&take=500")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "startDate must be a valid ISO 8601 date string",
            "endDate must be a valid ISO 8601 date string",
            "take must not be greater than 100"
        ])
    );
    let res = app
        .get("/google-calendar/calendar?year=2026&month=5")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok();
    let days = res.data()["days"].as_array().unwrap().clone();
    assert_eq!(days.len(), 31);
    assert_eq!(days[19]["date"], "2026-05-20");
    assert_eq!(days[19]["events"].as_array().unwrap().len(), 3);
    assert!(days[0]["events"].as_array().unwrap().is_empty());
    assert_eq!(res.data()["googleConnected"], true);
    assert!(res.text().contains(r#"{"year":2026,"month":5,"googleConnected":true,"days":[{"date":"2026-05-01","events":[],"onLeave":"#));
    app.get("/google-calendar/calendar?year=1999&month=13")
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(400);

    // Hapus: event Google (404 diabaikan), 500 gagal, lokal tanpa panggilan.
    mock.clear_log();
    mock.on("DELETE", &format!("{EVENTS}/g1"), 500, json!({}));
    let res = app
        .delete(&format!("/google-calendar/events/{g_uuid}"))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(res.json["message"], "Request failed with status code 500");
    mock.on("DELETE", &format!("{EVENTS}/g1"), 404, json!({}));
    let res = app
        .delete(&format!("/google-calendar/events/{g_uuid}"))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(
        res.json,
        json!({"success": true, "message": "Event deleted", "data": null})
    );
    assert_eq!(
        q(
            &mock.requests_to("DELETE", &format!("{EVENTS}/g1"))[0],
            "sendUpdates"
        )
        .as_deref(),
        Some("all")
    );
    mock.clear_log();
    app.delete(&format!("/google-calendar/events/{local_uuid}"))
        .token(&fx.admin.token)
        .send()
        .await
        .assert_ok();
    assert!(mock.requests().is_empty());
    let res = app
        .delete(&format!("/google-calendar/events/{local_uuid}"))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(res.json["message"], "Event not found");
    reset(&app, &fx).await;
}

#[tokio::test]
async fn it_int_gcal_user_calendar() {
    let _g = SERIAL.lock().await;
    let Some((app, mock, fx)) = setup().await else {
        return;
    };
    let uid = fx.staff.id;
    let res = app
        .get("/google-calendar/user/auth-url")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok();
    let url = res.json["data"]["url"].as_str().unwrap().to_owned();
    assert!(url.contains("scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcalendar.events+https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fuserinfo.email"));
    let parsed = parse_query(url.split_once('?').map(|p| p.1));
    let state = query_get(&parsed, "state")
        .and_then(QVal::as_str)
        .unwrap()
        .to_owned();

    mock.on(
        "POST",
        "/token",
        200,
        json!({"access_token": "uat", "refresh_token": "urt", "expires_in": 3600}),
    );
    mock.on("GET", "/oauth2/v2/userinfo", 500, json!({}));
    mock.on("GET", EVENTS, 200, items());
    let res = app
        .get(&format!(
            "/google-calendar/user/callback?code=k&state={state}"
        ))
        .send()
        .await;
    assert_eq!(
        location(&res),
        "http://fe.test/settings/integrations?connected=google_calendar_personal"
    );
    let db = app.db().clone();
    wait_until(|| {
        let db = db.clone();
        async move {
            let st: Option<String> =
                sqlx::query_scalar(r#"SELECT "googleCalendarSyncToken" FROM users WHERE id = $1"#)
                    .bind(uid)
                    .fetch_one(&db)
                    .await
                    .unwrap();
            st.as_deref() == Some("st-new")
        }
    })
    .await;
    let list = &mock.requests_to("GET", EVENTS)[0];
    assert_eq!(q(list, "orderBy").as_deref(), Some("startTime"));
    let ids: Vec<String> = sqlx::query_scalar(
        r#"SELECT "googleEventId" FROM "googleCalendarEvent" WHERE "userId" = $1 AND "calendarId" = $2 ORDER BY 1"#,
    )
    .bind(uid)
    .bind(format!("personal:{uid}"))
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(ids, vec![format!("ev1:u{uid}"), format!("ev2:u{uid}")]);

    let res = app
        .get("/google-calendar/user/status")
        .token(&fx.staff.token)
        .send()
        .await;
    assert_eq!(
        res.json,
        json!({"success": true, "message": "Success", "data": {"connected": true, "email": null}})
    );

    // Buat event pribadi: Google 200, lalu 401 → lokal.
    mock.on(
        "POST",
        EVENTS,
        200,
        json!({"id": "p1", "htmlLink": "https://cal/p1"}),
    );
    let ev = json!({"title": "Dokter", "startAt": "2026-05-20T14:00:00+07:00", "endAt": "2026-05-20T15:00:00+07:00"});
    let res = app
        .post("/google-calendar/user/events")
        .token(&fx.staff.token)
        .json(&ev)
        .send()
        .await;
    res.assert_status(201);
    assert_eq!(
        res.json["message"],
        "Personal event created and synced to Google Calendar"
    );
    assert_eq!(res.data()["googleEventId"], format!("p1:u{uid}"));
    assert!(res.data().get("createdBy").is_none());
    let p_uuid = res.data()["id"].as_str().unwrap().to_owned();
    mock.on("POST", EVENTS, 401, json!({}));
    let res = app
        .post("/google-calendar/user/events")
        .token(&fx.staff.token)
        .json(&ev)
        .send()
        .await;
    assert_eq!(
        res.json["message"],
        "Personal event created locally (Google Calendar not connected)"
    );

    // Daftar.
    let res = app
        .get("/google-calendar/user/events?startDate=2026-05-01&endDate=2026-06-01")
        .token(&fx.staff.token)
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.json["meta"]["total"], 4);
    assert!(res.text().contains(r#""source":"personal","origin":"#));
    // Staff lain tidak melihatnya.
    let res = app
        .get("/google-calendar/user/events?startDate=2026-05-01&endDate=2026-06-01")
        .token(&fx.staff2.token)
        .send()
        .await;
    assert_eq!(res.json["meta"]["total"], 0);

    // Hapus: event pribadi dihapus dari kalender `primary` dengan id Google asli (tanpa `:u<id>`).
    let path = "/calendar/v3/calendars/primary/events/p1".to_owned();
    mock.on("DELETE", "/calendar/v3/calendars/*", 410, json!({}));
    let res = app
        .delete(&format!("/google-calendar/user/events/{p_uuid}"))
        .token(&fx.staff.token)
        .send()
        .await;
    assert_eq!(res.json["message"], "Request failed with status code 410");
    let del = mock
        .requests()
        .into_iter()
        .rfind(|r| r.method == "DELETE")
        .unwrap();
    assert_eq!(del.path, path);
    mock.on("DELETE", "/calendar/v3/calendars/*", 404, json!({}));
    app.delete(&format!("/google-calendar/user/events/{p_uuid}"))
        .token(&fx.staff2.token)
        .send()
        .await
        .assert_status(200);
    let res = app
        .delete(&format!("/google-calendar/user/events/{p_uuid}"))
        .token(&fx.staff.token)
        .send()
        .await;
    assert_eq!(
        res.json,
        json!({"success": true, "message": "Event deleted", "data": null})
    );

    // Disconnect: revoke (gagal pun tetap lanjut), event google dihapus, lokal tetap.
    mock.on("POST", "/revoke", 400, json!({"error": "invalid_token"}));
    let res = app
        .delete("/google-calendar/user/disconnect")
        .token(&fx.staff.token)
        .send()
        .await;
    assert_eq!(
        res.json,
        json!({"success": true, "message": "Personal Google Calendar disconnected", "data": null})
    );
    let left: Vec<String> =
        sqlx::query_scalar(r#"SELECT source FROM "googleCalendarEvent" WHERE "userId" = $1"#)
            .bind(uid)
            .fetch_all(app.db())
            .await
            .unwrap();
    assert_eq!(left, vec!["local".to_owned()]);
    reset(&app, &fx).await;
}

#[tokio::test]
async fn it_int_gcal_cron_jobs() {
    let _g = SERIAL.lock().await;
    let Some((app, mock, fx)) = setup().await else {
        return;
    };
    let jobs = mz_int_gcal::cron_jobs();
    assert_eq!(
        jobs.iter().map(|j| j.name).collect::<Vec<_>>(),
        vec![
            "scheduled-sync-all-companies",
            "scheduled-renew-webhook-channels"
        ]
    );
    connect_company(&app, &fx, None).await;
    app.exec(&format!(
        r#"UPDATE company SET "googleCalendarWebhookChannelId" = 'old-ch',
             "googleCalendarWebhookResourceId" = 'old-res',
             "googleCalendarWebhookExpiry" = NOW() + interval '2 hours' WHERE id = {}"#,
        fx.id
    ))
    .await;
    let exp = (chrono::Utc::now().timestamp_millis() + 7 * 86_400_000).to_string();
    mock.on("POST", "/calendar/v3/channels/stop", 404, json!({}));
    mock.on(
        "POST",
        "/calendar/v3/calendars/primary/events/watch",
        200,
        json!({"id": "new-ch", "resourceId": "new-res", "expiration": exp}),
    );
    mock.on("GET", EVENTS, 200, items());
    mock.on(
        "POST",
        "/token",
        401,
        json!({"error": "unauthorized_client"}),
    );
    mz_jobs::run_now(&app.state, &jobs[1]).await.unwrap();
    let ch: Option<String> = app
        .scalar(&format!(
            r#"SELECT "googleCalendarWebhookChannelId" FROM company WHERE id = {}"#,
            fx.id
        ))
        .await;
    assert_eq!(ch.as_deref(), Some("new-ch"));
    assert_eq!(
        mock.requests_to("POST", "/calendar/v3/channels/stop")[0].json(),
        json!({"id": "old-ch", "resourceId": "old-res"})
    );

    mz_jobs::run_now(&app.state, &jobs[0]).await.unwrap();
    let st: Option<String> = app
        .scalar(&format!(
            r#"SELECT "googleCalendarSyncToken" FROM company WHERE id = {}"#,
            fx.id
        ))
        .await;
    assert_eq!(st.as_deref(), Some("st-new"));
    reset(&app, &fx).await;
}
