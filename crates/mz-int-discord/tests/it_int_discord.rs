//! Uji integrasi 17 route `integrations/discord/*` lewat HTTP (gerbang lengkap + layer
//! audit) terhadap Postgres nyata, Bot API Discord diganti `MockServer`.
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-int-discord --test it_int_discord`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::Duration;

use axum::Router;
use mz_core::AppState;
use mz_int_common::mock::MockServer;
use mz_testkit::{Company, TestApp};
use serde_json::{Value, json};
use tokio::sync::Mutex;

/// Semua test memakai company fixture yang sama: dijalankan berurutan.
static SERIAL: Mutex<()> = Mutex::const_new(());

const GUILD: &str = "111111111111111111";
const GUILD2: &str = "999999999999999999";
const CH1: &str = "222222222222222222";
const CH2: &str = "333333333333333333";
const CH_VOICE: &str = "444444444444444444";
const MEMBER1: &str = "555555555555555555";
const MEMBER2: &str = "666666666666666666";
const FE: &str = "http://fe.test";

fn build(state: &AppState) -> Router {
    mz_int_discord::router(state)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

struct Ctx {
    app: TestApp,
    mock: MockServer,
    c: Company,
    /// Dua project aktif milik company fixture.
    p1: String,
    p2: String,
}

async fn setup() -> Option<Ctx> {
    let mock = MockServer::start().await;
    let base = mock.url();
    let app = TestApp::with_vars(
        build,
        &[
            ("MZ_DISCORD_API_BASE", &base),
            ("DISCORD_BOT_TOKEN", "test-bot"),
            ("DISCORD_CLIENT_ID", "cid"),
            (
                "DISCORD_REDIRECT_URI",
                "http://be.test/integrations/discord/callback",
            ),
            ("FRONTEND_URL", FE),
        ],
    )
    .await?;
    let c = app.company().await;
    app.exec(&format!(
        r#"UPDATE company SET "discordGuildId" = NULL, "discordGuildName" = NULL,
              "discordDefaultTaskChannelId" = NULL, "discordDefaultTaskChannelName" = NULL;
           DELETE FROM "projectDiscordMapping";
           DELETE FROM "userDiscordMapping";
           INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
           SELECT al.id, 'integrations', a::"PermissionAction" FROM "accessLevel" al
           CROSS JOIN (VALUES ('READ'), ('WRITE'), ('DELETE')) AS f(a)
           WHERE al."isSystem" AND al.code IN ('SYSTEM_ADMIN', 'SUPER_ADMIN', 'ADMIN', 'MANAGER')
           ON CONFLICT DO NOTHING;
           INSERT INTO project (code, name, "screenshotPerHour", "projectTypeId", "startDate", status)
           SELECT 'DISCORD-IT-2', 'Discord P2', "screenshotPerHour", "projectTypeId", "startDate", 1
           FROM project p WHERE p.status = 1 AND EXISTS (SELECT 1 FROM "projectCompany" pc
                 WHERE pc."projectId" = p.id AND pc."companyId" = {0} AND pc.status = 1)
             AND NOT EXISTS (SELECT 1 FROM project WHERE code = 'DISCORD-IT-2')
           ORDER BY p.id LIMIT 1;
           INSERT INTO "projectCompany" ("projectId", "companyId", status)
           SELECT id, {0}, 1 FROM project WHERE code = 'DISCORD-IT-2'
             AND NOT EXISTS (SELECT 1 FROM "projectCompany" pc JOIN project p ON p.id = pc."projectId"
                             WHERE p.code = 'DISCORD-IT-2');"#,
        c.id
    ))
    .await;
    let projects: Vec<String> = sqlx::query_scalar(
        r#"SELECT p.uuid::text FROM project p WHERE p.status = 1 AND EXISTS (
             SELECT 1 FROM "projectCompany" pc WHERE pc."projectId" = p.id
               AND pc."companyId" = $1 AND pc.status = 1)
           ORDER BY p.id LIMIT 2"#,
    )
    .bind(c.id)
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(projects.len(), 2, "butuh dua project aktif");
    Some(Ctx {
        p1: projects[0].clone(),
        p2: projects[1].clone(),
        app,
        mock,
        c,
    })
}

async fn connect_guild(ctx: &Ctx) {
    ctx.app
        .exec(&format!(
            r#"UPDATE company SET "discordGuildId" = '{GUILD}', "discordGuildName" = 'G' WHERE id = {}"#,
            ctx.c.id
        ))
        .await;
}

fn channels_body() -> Value {
    json!([
        { "id": CH2, "name": "two", "type": 0, "position": 2, "parent_id": "777777777777777777" },
        { "id": CH_VOICE, "name": "voice", "type": 2, "position": 0 },
        { "id": CH1, "name": "one", "type": 5, "position": 1 },
        { "id": "888888888888888888", "name": "nopos", "type": 0 }
    ])
}

fn location(res: &mz_testkit::TestResponse) -> String {
    res.headers
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

async fn audit_rows(app: &TestApp, company: i32) -> i64 {
    let mut n = 0;
    for _ in 0..40 {
        n = app
            .scalar(&format!(
                r#"SELECT count(*) FROM "auditLog" WHERE "entityType" = 'company' AND "entityId" = {company}
                     AND "changedFields"::text LIKE '%discord%'"#
            ))
            .await;
        if n > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    n
}

#[tokio::test]
async fn it_int_discord_connect_callback_status() {
    let _g = SERIAL.lock().await;
    let Some(ctx) = setup().await else { return };
    let (app, mock, c) = (&ctx.app, &ctx.mock, &ctx.c);
    let admin = &c.admin.token;

    // Tanpa token → 401; STAFF → 401 `Access denied: insufficient permissions` (RolesGuard).
    app.get("/integrations/discord/status")
        .send()
        .await
        .assert_status(401);
    app.get("/integrations/discord/status")
        .token(&c.staff.token)
        .send()
        .await
        .assert_status(401);

    let res = app
        .get("/integrations/discord/status")
        .token(admin)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"Success","data":{"connected":false,"guildId":null,"guildName":null}}"#
    );

    // POST connect → 200 + URL otorisasi.
    let res = app
        .post("/integrations/discord/connect")
        .token(admin)
        .send()
        .await;
    res.assert_status(200);
    let url = res.json["data"]["url"].as_str().unwrap().to_owned();
    assert!(res.text().starts_with(r#"{"success":true,"data":{"url":"#));
    let prefix = "https://discord.com/oauth2/authorize?response_type=code&client_id=cid\
                  &scope=bot%20applications.commands&permissions=84992\
                  &redirect_uri=http%3A%2F%2Fbe.test%2Fintegrations%2Fdiscord%2Fcallback&state=";
    assert!(url.starts_with(prefix), "{url}");
    let state = url[prefix.len()..].to_owned();

    // Discord membatalkan.
    let res = app
        .get("/integrations/discord/callback?error=access_denied")
        .send()
        .await;
    res.assert_status(302);
    assert_eq!(
        location(&res),
        format!("{FE}/integrations/discord?status=error&message=access_denied")
    );
    assert_eq!(
        res.text(),
        format!(
            "Found. Redirecting to {FE}/integrations/discord?status=error&message=access_denied"
        )
    );
    // Tanpa guild_id → pesan bawaan.
    let res = app
        .get(&format!("/integrations/discord/callback?state={state}"))
        .send()
        .await;
    assert_eq!(
        location(&res),
        format!(
            "{FE}/integrations/discord?status=error&message=Discord%20authorization%20was%20cancelled%20or%20failed"
        )
    );
    // `error=` kosong: `'' ?? default` = ''.
    let res = app
        .get("/integrations/discord/callback?error=")
        .send()
        .await;
    assert_eq!(
        location(&res),
        format!("{FE}/integrations/discord?status=error&message=")
    );
    // State palsu / kedaluwarsa / tipe salah.
    let expired = app.token_with(json!({
        "userId": c.admin.id, "type": "discord-connect", "iat": 1, "exp": 2
    }));
    let wrong_type = app.token_with(json!({ "userId": c.admin.id, "type": "jira-connect" }));
    for st in ["garbage", expired.as_str(), wrong_type.as_str()] {
        let res = app
            .get(&format!(
                "/integrations/discord/callback?guild_id={GUILD}&state={st}"
            ))
            .send()
            .await;
        assert_eq!(
            location(&res),
            format!(
                "{FE}/integrations/discord?status=error&message=Invalid%20or%20expired%20authorization%20state"
            )
        );
    }
    // Guild id bukan snowflake.
    let res = app
        .get(&format!(
            "/integrations/discord/callback?guild_id=123&state={state}"
        ))
        .send()
        .await;
    assert_eq!(
        location(&res),
        format!("{FE}/integrations/discord?status=error&message=Invalid%20Discord%20server%20id")
    );
    // Discord 404 untuk guild → pesan tak terduga, company tidak berubah.
    mock.on(
        "GET",
        &format!("/guilds/{GUILD}"),
        404,
        json!({ "message": "Unknown Guild", "code": 10004 }),
    );
    let res = app
        .get(&format!(
            "/integrations/discord/callback?guild_id={GUILD}&state={state}"
        ))
        .send()
        .await;
    assert_eq!(
        location(&res),
        format!(
            "{FE}/integrations/discord?status=error&message=An%20unexpected%20error%20occurred"
        )
    );

    // Sukses.
    mock.on(
        "GET",
        &format!("/guilds/{GUILD}"),
        200,
        json!({ "id": GUILD, "name": "My Server" }),
    );
    mock.clear_log();
    let res = app
        .get(&format!(
            "/integrations/discord/callback?guild_id={GUILD}&state={state}"
        ))
        .send()
        .await;
    res.assert_status(302);
    assert_eq!(
        location(&res),
        format!("{FE}/integrations/discord?status=connected")
    );
    let reqs = mock.requests_to("GET", &format!("/guilds/{GUILD}"));
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].header("authorization"), Some("Bot test-bot"));
    let (gid, gname): (Option<String>, Option<String>) =
        sqlx::query_as(r#"SELECT "discordGuildId", "discordGuildName" FROM company WHERE id = $1"#)
            .bind(c.id)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(gid.as_deref(), Some(GUILD));
    assert_eq!(gname.as_deref(), Some("My Server"));
    assert!(
        audit_rows(app, c.id).await > 0,
        "update company harus diaudit"
    );

    let res = app
        .get("/integrations/discord/status")
        .token(admin)
        .send()
        .await;
    assert_eq!(
        res.text(),
        format!(
            r#"{{"success":true,"message":"Success","data":{{"connected":true,"guildId":"{GUILD}","guildName":"My Server"}}}}"#
        )
    );
    // Ekspor untuk agregat `/integrations`.
    let st = mz_int_discord::get_connection_status(&app.state, c.admin.id).await;
    assert!(st.success && st.data.is_some_and(|d| d.connected));
    let st = mz_int_discord::get_connection_status(&app.state, c.staff.id).await;
    assert_eq!(st.message, "Company not found");

    // Guild milik company lain → ditolak.
    app.exec(&format!(
        r#"UPDATE company SET "discordGuildId" = '{GUILD2}' WHERE id = (SELECT min(id) FROM company WHERE id <> {})"#,
        c.id
    ))
    .await;
    let res = app
        .get(&format!(
            "/integrations/discord/callback?guild_id={GUILD2}&state={state}"
        ))
        .send()
        .await;
    assert_eq!(
        location(&res),
        format!(
            "{FE}/integrations/discord?status=error&message=This%20Discord%20server%20is%20already%20linked%20to%20another%20account"
        )
    );
    app.exec(&format!(
        r#"UPDATE company SET "discordGuildId" = NULL WHERE "discordGuildId" = '{GUILD2}'"#
    ))
    .await;
}

#[tokio::test]
async fn it_int_discord_channels_links_notify() {
    let _g = SERIAL.lock().await;
    let Some(ctx) = setup().await else { return };
    let (app, mock, c) = (&ctx.app, &ctx.mock, &ctx.c);
    let admin = &c.admin.token;

    // Belum terhubung.
    let res = app
        .get("/integrations/discord/channels")
        .token(admin)
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":false,"message":"Discord not connected","data":null}"#
    );
    connect_guild(&ctx).await;

    // Discord 429 / 500 → pesan tak terduga (HTTP 200).
    mock.on(
        "GET",
        &format!("/guilds/{GUILD}/channels"),
        429,
        json!({ "message": "You are being rate limited.", "retry_after": 1.0 }),
    );
    let res = app
        .get("/integrations/discord/channels")
        .token(admin)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(
        res.text(),
        r#"{"success":false,"message":"An unexpected error occurred","data":null}"#
    );

    mock.on(
        "GET",
        &format!("/guilds/{GUILD}/channels"),
        200,
        channels_body(),
    );
    let res = app
        .get("/integrations/discord/channels")
        .token(admin)
        .send()
        .await;
    assert_eq!(
        res.text(),
        format!(
            r#"{{"success":true,"message":"Success","data":[{{"id":"888888888888888888","name":"nopos","type":0,"position":null,"parentId":null,"linkedProject":null}},{{"id":"{CH1}","name":"one","type":5,"position":1,"parentId":null,"linkedProject":null}},{{"id":"{CH2}","name":"two","type":0,"position":2,"parentId":"777777777777777777","linkedProject":null}}]}}"#
        )
    );

    // Validasi DTO.
    let res = app
        .post("/integrations/discord/channels/link")
        .token(admin)
        .json(
            &json!({ "projectUuid": ctx.p1, "discordChannelId": "abc", "discordChannelName": "x" }),
        )
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!(["discordChannelId must be a valid Discord ID"])
    );
    // Channel bukan milik guild / bukan teks.
    for ch in ["121212121212121212", CH_VOICE] {
        let res = app
            .post("/integrations/discord/channels/link")
            .token(admin)
            .json(&json!({ "projectUuid": ctx.p1, "discordChannelId": ch, "discordChannelName": "x" }))
            .send()
            .await;
        assert_eq!(
            res.json["message"],
            "Channel not found in your connected Discord server"
        );
    }
    // Project tidak ada.
    let res = app
        .post("/integrations/discord/channels/link")
        .token(admin)
        .json(&json!({ "projectUuid": "00000000-0000-4000-8000-000000000000", "discordChannelId": CH1, "discordChannelName": "x" }))
        .send()
        .await;
    assert_eq!(res.json["message"], "Project not found");

    let res = app
        .post("/integrations/discord/channels/link")
        .token(admin)
        .json(&json!({
            "projectUuid": ctx.p1, "discordChannelId": CH1, "discordChannelName": "ignored",
            "notificationTypes": ["leave_approved", "leave_rejected"]
        }))
        .send()
        .await;
    res.assert_status(200).assert_ok();
    let d = res.data();
    assert_eq!(d["discordChannelName"], "one");
    assert_eq!(d["projectUuid"], ctx.p1.as_str());
    assert!(res.text().contains(
        r#""discordChannelId":"222222222222222222","discordChannelName":"one","notificationTypes":["leave_approved","leave_rejected"]}"#
    ));

    // Channel yang sama ke project lain → konflik.
    let pname: String = app
        .scalar(&format!(
            "SELECT name FROM project WHERE uuid = '{}'",
            ctx.p1
        ))
        .await;
    let res = app
        .post("/integrations/discord/channels/link")
        .token(admin)
        .json(&json!({ "projectUuid": ctx.p2, "discordChannelId": CH1, "discordChannelName": "x" }))
        .send()
        .await;
    assert_eq!(
        res.json["message"],
        format!("This Discord channel is already linked to project '{pname}'")
    );

    let res = app
        .get("/integrations/discord/channels")
        .token(admin)
        .send()
        .await;
    assert_eq!(res.data()[1]["linkedProject"]["uuid"], ctx.p1.as_str());

    let res = app
        .get("/integrations/discord/linked-channels")
        .token(admin)
        .send()
        .await;
    res.assert_ok();
    let items = res.data().as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["discordChannelId"], CH1);
    assert!(items[0]["linkedAt"].as_str().unwrap().ends_with('Z'));

    // Notify: sukses, isi embed.
    mock.on(
        "POST",
        &format!("/channels/{CH1}/messages"),
        200,
        json!({ "id": "1" }),
    );
    mock.clear_log();
    let res = app
        .post(&format!("/integrations/discord/projects/{}/notify", ctx.p1))
        .token(admin)
        .json(&json!({ "title": "Task Updated", "message": "Moved", "color": 255 }))
        .send()
        .await;
    assert_eq!(
        res.text(),
        format!(
            r#"{{"success":true,"message":"Notification sent successfully","data":{{"channelId":"{CH1}","channelName":"one"}}}}"#
        )
    );
    let sent = mock.requests_to("POST", &format!("/channels/{CH1}/messages"));
    assert_eq!(sent.len(), 1);
    let body = sent[0].json();
    assert_eq!(body["embeds"][0]["title"], "Task Updated");
    assert_eq!(body["embeds"][0]["description"], "Moved");
    assert_eq!(body["embeds"][0]["color"], 255);
    assert!(body.get("content").is_none());
    assert_eq!(sent[0].header("authorization"), Some("Bot test-bot"));

    // Validasi color.
    let res = app
        .post(&format!("/integrations/discord/projects/{}/notify", ctx.p1))
        .token(admin)
        .json(&json!({ "title": "t", "message": "m", "color": -1 }))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!(["color must not be less than 0"])
    );

    // Test-notify: warna bawaan.
    mock.clear_log();
    let res = app
        .post(&format!(
            "/integrations/discord/projects/{}/test-notify",
            ctx.p1
        ))
        .token(admin)
        .send()
        .await;
    res.assert_ok();
    let body = mock.requests_to("POST", &format!("/channels/{CH1}/messages"))[0].json();
    assert_eq!(body["embeds"][0]["title"], "Discord Integration Test");
    assert_eq!(body["embeds"][0]["color"], 5_763_719);

    // Discord 500 / 403 saat kirim → pesan gagal kirim.
    for status in [500, 403] {
        mock.on(
            "POST",
            &format!("/channels/{CH1}/messages"),
            status,
            json!({ "message": "x" }),
        );
        let res = app
            .post(&format!(
                "/integrations/discord/projects/{}/test-notify",
                ctx.p1
            ))
            .token(admin)
            .send()
            .await;
        assert_eq!(
            res.text(),
            r#"{"success":false,"message":"Failed to send notification to Discord","data":null}"#
        );
    }
    // Project tanpa link.
    let res = app
        .post(&format!(
            "/integrations/discord/projects/{}/test-notify",
            ctx.p2
        ))
        .token(admin)
        .send()
        .await;
    assert_eq!(
        res.json["message"],
        "Project not found or not linked to a Discord channel"
    );

    // Unlink.
    let res = app
        .delete(&format!("/integrations/discord/channels/{}/link", ctx.p1))
        .token(admin)
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"Project unlinked from Discord channel successfully","data":null}"#
    );
    let res = app
        .delete(&format!("/integrations/discord/channels/{}/link", ctx.p1))
        .token(admin)
        .send()
        .await;
    assert_eq!(
        res.json["message"],
        "No active Discord channel link found for this project"
    );

    // Link ulang (upsert) tanpa `notificationTypes` → JSON null seperti Prisma.
    let res = app
        .post("/integrations/discord/channels/link")
        .token(admin)
        .json(&json!({ "projectUuid": ctx.p1, "discordChannelId": CH2, "discordChannelName": "x" }))
        .send()
        .await;
    res.assert_ok();
    assert_eq!(res.data()["notificationTypes"], Value::Null);
    assert_eq!(res.data()["discordChannelId"], CH2);
    let (n, stored): (i64, String) = sqlx::query_as(
        r#"SELECT count(*), max("notificationTypes"::text) FROM "projectDiscordMapping"
           WHERE "companyId" = $1 AND status = 1"#,
    )
    .bind(c.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!((n, stored.as_str()), (1, "null"));
}

#[tokio::test]
async fn it_int_discord_members_users_tasks_disconnect() {
    let _g = SERIAL.lock().await;
    let Some(ctx) = setup().await else { return };
    let (app, mock, c) = (&ctx.app, &ctx.mock, &ctx.c);
    let admin = &c.admin.token;
    connect_guild(&ctx).await;

    // Members: bot tidak bisa akses guild.
    mock.on(
        "GET",
        &format!("/guilds/{GUILD}"),
        403,
        json!({ "message": "Missing Access" }),
    );
    let res = app
        .get("/integrations/discord/members")
        .token(admin)
        .send()
        .await;
    assert_eq!(
        res.json["message"],
        "Bot cannot access the connected Discord server. Make sure the bot is still a member of the server."
    );
    mock.on(
        "GET",
        &format!("/guilds/{GUILD}"),
        200,
        json!({ "name": "G" }),
    );
    for (status, msg) in [
        (
            403,
            "Bot does not have permission to list members. Enable the \"Server Members Intent\" in the Discord Developer Portal for your bot.",
        ),
        (401, "Discord bot token is invalid or missing"),
        (502, "An unexpected error occurred"),
    ] {
        mock.on(
            "GET",
            &format!("/guilds/{GUILD}/members"),
            status,
            json!({ "message": "x" }),
        );
        let res = app
            .get("/integrations/discord/members")
            .token(admin)
            .send()
            .await;
        res.assert_status(200);
        assert_eq!(res.json["message"], msg);
    }
    mock.on(
        "GET",
        &format!("/guilds/{GUILD}/members"),
        200,
        json!([
            { "nick": null, "user": { "id": MEMBER1, "username": "john", "global_name": "John", "avatar": "abc" } },
            { "user": { "id": "1", "username": "botty", "bot": true } },
            { "nick": "Jany", "user": { "id": MEMBER2, "username": "jane", "avatar": null } }
        ]),
    );

    // Link user (STAFF fixture) ke MEMBER1.
    let res = app
        .post("/integrations/discord/users/link")
        .token(admin)
        .json(&json!({ "userUuid": c.staff.uuid, "discordUserId": MEMBER1, "discordAvatarUrl": "https://evil.com/a.png" }))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!(["discordAvatarUrl must be a URL address"])
    );
    let res = app
        .post("/integrations/discord/users/link")
        .token(admin)
        .json(&json!({
            "userUuid": c.staff.uuid, "discordUserId": MEMBER1, "discordUsername": "john",
            "discordAvatarUrl": "https://cdn.discordapp.com/avatars/x/abc.png"
        }))
        .send()
        .await;
    res.assert_ok();
    let staff_name: String = app
        .scalar(&format!("SELECT name FROM users WHERE id = {}", c.staff.id))
        .await;
    assert_eq!(
        res.text(),
        format!(
            r#"{{"success":true,"message":"User linked to Discord member successfully","data":{{"uuid":"{}","userUuid":"{}","userName":{},"discordUserId":"{MEMBER1}","discordUsername":"john","discordDisplayName":null}}}}"#,
            res.data()["uuid"].as_str().unwrap(),
            c.staff.uuid,
            Value::from(staff_name.clone())
        )
    );
    // Member yang sama ke user lain → konflik.
    let res = app
        .post("/integrations/discord/users/link")
        .token(admin)
        .json(&json!({ "userUuid": c.staff2.uuid, "discordUserId": MEMBER1 }))
        .send()
        .await;
    assert_eq!(
        res.json["message"],
        format!("This Discord member is already linked to user '{staff_name}'")
    );
    let res = app
        .post("/integrations/discord/users/link")
        .token(admin)
        .json(&json!({ "userUuid": "00000000-0000-4000-8000-000000000000", "discordUserId": MEMBER2 }))
        .send()
        .await;
    assert_eq!(res.json["message"], "User not found in this company");

    let res = app
        .get("/integrations/discord/members")
        .token(admin)
        .send()
        .await;
    res.assert_ok();
    let m = res.data().as_array().unwrap();
    assert_eq!(m.len(), 2);
    assert_eq!(
        m[0]["discordAvatarUrl"],
        format!("https://cdn.discordapp.com/avatars/{MEMBER1}/abc.png")
    );
    assert_eq!(m[0]["discordDisplayName"], "John");
    assert_eq!(m[0]["linkedUser"]["uuid"], c.staff.uuid.as_str());
    assert_eq!(m[1]["discordDisplayName"], "Jany");
    assert_eq!(m[1]["discordAvatarUrl"], Value::Null);
    assert_eq!(m[1]["linkedUser"], Value::Null);
    let members_req = mock.requests_to("GET", &format!("/guilds/{GUILD}/members"));
    assert_eq!(members_req.last().unwrap().query, "limit=1000");

    // linked-users: mapping aktif + join users; `userEmail` = `users.username`.
    let res = app
        .get("/integrations/discord/linked-users")
        .token(admin)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["success"], true);
    let lu = res.data().as_array().unwrap();
    assert_eq!(lu.len(), 1);
    assert_eq!(lu[0]["userUuid"], c.staff.uuid.as_str());
    let (uname, uusername): (String, String) =
        sqlx::query_as("SELECT name, username FROM users WHERE uuid = $1::uuid")
            .bind(&c.staff.uuid)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(lu[0]["userName"], uname.as_str());
    assert_eq!(lu[0]["userUsername"], uusername.as_str());
    assert_eq!(lu[0]["userEmail"], uusername.as_str());
    assert_eq!(lu[0]["discordUserId"], MEMBER1);
    let keys: Vec<&str> = lu[0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "uuid",
            "userUuid",
            "userName",
            "userUsername",
            "userEmail",
            "discordUserId",
            "discordUsername",
            "discordDisplayName",
            "discordAvatarUrl",
            "linkedAt"
        ]
    );

    // Default task channel.
    let res = app
        .get("/integrations/discord/settings/default-task-channel")
        .token(admin)
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"Success","data":{"channelId":null,"channelName":null}}"#
    );
    let res = app
        .put("/integrations/discord/settings/default-task-channel")
        .token(admin)
        .json(&json!({ "channelId": "12" }))
        .send()
        .await;
    assert_eq!(res.json["message"], "Invalid Discord channel id");
    let res = app
        .put("/integrations/discord/settings/default-task-channel")
        .token(admin)
        .json(&json!({ "channelId": 5 }))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(res.json["message"], json!(["channelId must be a string"]));
    mock.on(
        "GET",
        &format!("/guilds/{GUILD}/channels"),
        503,
        Value::Null,
    );
    let res = app
        .put("/integrations/discord/settings/default-task-channel")
        .token(admin)
        .json(&json!({ "channelId": CH2 }))
        .send()
        .await;
    assert_eq!(res.json["message"], "An unexpected error occurred");
    mock.on(
        "GET",
        &format!("/guilds/{GUILD}/channels"),
        200,
        channels_body(),
    );
    let res = app
        .put("/integrations/discord/settings/default-task-channel")
        .token(admin)
        .json(&json!({ "channelId": CH2 }))
        .send()
        .await;
    assert_eq!(
        res.text(),
        format!(
            r#"{{"success":true,"message":"Default task channel set successfully","data":{{"channelId":"{CH2}","channelName":"two"}}}}"#
        )
    );

    // post-tasks: STAFF ditolak RolesGuard (401); MANAGER boleh.
    let body = json!({ "projects": [
        { "projectUuid": ctx.p1, "tasks": ["Fix login bug", "Write tests"] },
        { "projectUuid": "00000000-0000-4000-8000-000000000000", "tasks": ["x"] }
    ] });
    app.post("/integrations/discord/post-tasks")
        .token(&c.staff.token)
        .json(&body)
        .send()
        .await
        .assert_status(401);
    let res = app
        .post("/integrations/discord/post-tasks")
        .token(admin)
        .json(&json!({ "projects": [{ "projectUuid": "nope", "tasks": [] }] }))
        .send()
        .await;
    res.assert_status(400);
    assert_eq!(
        res.json["message"],
        json!([
            "projects.0.projectUuid must be a UUID",
            "projects.0.tasks must contain at least 1 elements"
        ])
    );
    // Admin ter-link → mention.
    app.exec(&format!(
        r#"INSERT INTO "userDiscordMapping" ("userId", "companyId", "discordUserId", status)
           VALUES ({}, {}, '{MEMBER2}', 1)"#,
        c.admin.id, c.id
    ))
    .await;
    mock.on(
        "POST",
        &format!("/channels/{CH2}/messages"),
        200,
        json!({ "id": "1" }),
    );
    mock.clear_log();
    let res = app
        .post("/integrations/discord/post-tasks")
        .token(admin)
        .json(&body)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(
        res.text(),
        format!(
            r#"{{"success":false,"message":"1 posted, 1 failed","data":{{"results":[{{"projectUuid":"{}","success":true,"channelId":"{CH2}","channelName":"two","usingDefaultChannel":true,"taskCount":2,"mentionedUserCount":1}},{{"projectUuid":"00000000-0000-4000-8000-000000000000","success":false,"message":"Project not found"}}],"totalPosted":1,"totalFailed":1}}}}"#,
            ctx.p1
        )
    );
    let sent = mock.requests_to("POST", &format!("/channels/{CH2}/messages"));
    assert_eq!(sent.len(), 1);
    let b = sent[0].json();
    assert_eq!(b["content"], format!("<@{MEMBER2}>"));
    assert_eq!(
        b["embeds"][0]["description"],
        "1. Fix login bug\n2. Write tests"
    );
    assert!(b["embeds"][0]["title"].as_str().unwrap().starts_with("📋 "));
    assert!(sent[0].body.starts_with(r#"{"embeds":"#));

    // Discord gagal (429) → hasil per project gagal.
    mock.on(
        "POST",
        &format!("/channels/{CH2}/messages"),
        429,
        json!({ "retry_after": 1 }),
    );
    // MANAGER lolos RolesGuard dan sekarang juga dikenali `resolveCompanyId` (cakupan company).
    let one = json!({ "projects": [{ "projectUuid": ctx.p1, "tasks": ["a"] }] });
    let res = app
        .post("/integrations/discord/post-tasks")
        .token(&c.manager.token)
        .json(&one)
        .send()
        .await;
    res.assert_status(200);
    assert_eq!(res.json["message"], "0 posted, 1 failed");
    assert_eq!(
        res.data()["results"][0]["message"],
        "Failed to send message to Discord"
    );
    let res = app
        .post("/integrations/discord/post-tasks")
        .token(admin)
        .json(&one)
        .send()
        .await;
    assert_eq!(res.json["message"], "0 posted, 1 failed");
    assert_eq!(
        res.data()["results"][0]["message"],
        "Failed to send message to Discord"
    );

    // Unset default → project tanpa link.
    let res = app
        .put("/integrations/discord/settings/default-task-channel")
        .token(admin)
        .json(&json!({ "channelId": null }))
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"Default task channel unset successfully","data":{"channelId":null,"channelName":null}}"#
    );
    let res = app
        .post("/integrations/discord/post-tasks")
        .token(admin)
        .json(&json!({ "projects": [{ "projectUuid": ctx.p1, "tasks": ["a"] }] }))
        .send()
        .await;
    assert_eq!(
        res.data()["results"][0]["message"],
        "Project is not linked to a Discord channel"
    );

    // Unlink user.
    let res = app
        .delete(&format!(
            "/integrations/discord/users/{}/link",
            c.staff.uuid
        ))
        .token(admin)
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"User unlinked from Discord successfully","data":null}"#
    );
    let res = app
        .delete(&format!(
            "/integrations/discord/users/{}/link",
            c.staff.uuid
        ))
        .token(admin)
        .send()
        .await;
    assert_eq!(
        res.json["message"],
        "No active Discord link found for this user"
    );

    // Disconnect: bot gagal keluar guild (500) tetap sukses; mapping dinonaktifkan.
    app.exec(&format!(
        r#"INSERT INTO "projectDiscordMapping" ("projectId", "companyId", "discordChannelId", "discordChannelName", status)
           SELECT id, {}, '{CH1}', 'one', 1 FROM project WHERE uuid = '{}'"#,
        c.id, ctx.p1
    ))
    .await;
    mock.on(
        "DELETE",
        &format!("/users/@me/guilds/{GUILD}"),
        500,
        json!({ "message": "x" }),
    );
    mock.clear_log();
    let res = app
        .delete("/integrations/discord/disconnect")
        .token(admin)
        .send()
        .await;
    assert_eq!(
        res.text(),
        r#"{"success":true,"message":"Discord disconnected successfully","data":null}"#
    );
    assert_eq!(
        mock.requests_to("DELETE", &format!("/users/@me/guilds/{GUILD}"))
            .len(),
        1
    );
    let active: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "projectDiscordMapping" WHERE "companyId" = {} AND status = 1"#,
            c.id
        ))
        .await;
    assert_eq!(active, 0);
    let gid: Option<String> = app
        .scalar(&format!(
            r#"SELECT "discordGuildId" FROM company WHERE id = {}"#,
            c.id
        ))
        .await;
    assert_eq!(gid, None);
    // Sesudah disconnect tidak ada panggilan leave lagi.
    mock.clear_log();
    app.delete("/integrations/discord/disconnect")
        .token(admin)
        .send()
        .await
        .assert_ok();
    assert!(mock.requests().is_empty());
}
