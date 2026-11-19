//! Uji integrasi route baca `/v1/divisions*` (list, employees, my-scope, detail, tracking,
//! report) lewat HTTP terhadap Postgres nyata: jangkauan admin vs Leader vs Member, role yang
//! salah, gerbang pilot/paket, dan angka waktu.
//! Jalankan: `MZ_TEST_DATABASE_URL=... MZ_TEST_REQUIRE_DB=1 cargo test -p mz-divisions --test it_divisions_read`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{assert_code, assert_key_order, build, enable, make_division};
use mz_testkit::{Company, TestApp};
use serde_json::{Value, json};
use tokio::sync::{Mutex, MutexGuard};

/// Semua test memakai company fixture yang sama: dijalankan berurutan.
static SERIAL: Mutex<()> = Mutex::const_new(());

const TZ: &str = "Asia/Jakarta";

struct Ctx {
    app: TestApp,
    fx: Company,
    a_uuid: String,
    b_uuid: String,
    project_uuid: String,
    project_id: i32,
    _guard: MutexGuard<'static, ()>,
}

/// Divisi A: staff = LEADER, staff2 = MEMBER, admin = MEMBER. Divisi B: manager = LEADER.
async fn setup() -> Option<Ctx> {
    let guard = SERIAL.lock().await;
    let app = TestApp::new(build).await?;
    let fx = app.company().await;
    enable(&app, &fx).await;
    app.exec(&format!(
        r#"UPDATE company SET "timeZone" = '{TZ}', "workweekStartDay" = 1, "dataHistoryMonths" = 3
           WHERE id = {};"#,
        fx.id
    ))
    .await;
    let (_, a_uuid) = make_division(
        &app,
        &fx,
        "it-A",
        &[
            (fx.staff.id, "LEADER"),
            (fx.staff2.id, "MEMBER"),
            (fx.admin.id, "MEMBER"),
        ],
    )
    .await;
    let (_, b_uuid) = make_division(&app, &fx, "it-B", &[(fx.manager.id, "LEADER")]).await;
    let (project_id, project_uuid): (i32, String) = sqlx::query_as(
        r#"SELECT p.id, p.uuid::text FROM project p
           JOIN "projectCompany" pc ON pc."projectId" = p.id AND pc."companyId" = $1 AND pc.status = 1
           ORDER BY p.id LIMIT 1"#,
    )
    .bind(fx.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    Some(Ctx {
        app,
        fx,
        a_uuid,
        b_uuid,
        project_uuid,
        project_id,
        _guard: guard,
    })
}

fn ids(v: &Value) -> Vec<i64> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn list_by_scope() {
    let Some(c) = setup().await else { return };
    let (app, fx) = (&c.app, &c.fx);

    // Admin: seluruh company + ringkasan.
    let r = app.get("/v1/divisions").token(&fx.admin.token).send().await;
    r.assert_status(200).assert_no_sql_error();
    let data = r.json["data"].as_array().unwrap();
    assert_eq!(data.len(), 2, "{}", r.text());
    assert_eq!(data[0]["name"], json!("it-A"));
    assert_eq!(data[1]["name"], json!("it-B"));
    assert_eq!(data[0]["memberCount"], json!(3));
    // Leader pertama = `leader` (deprecated), `membersPreview` memuat Leader dulu.
    assert_eq!(data[0]["leader"]["uuid"], json!(fx.staff.uuid));
    assert_eq!(data[0]["leaders"].as_array().unwrap().len(), 1);
    assert_eq!(data[0]["leader"]["avatarUrl"], Value::Null);
    assert_eq!(data[0]["membersPreview"][0]["uuid"], json!(fx.staff.uuid));
    let s = &r.json["summary"];
    assert_eq!(s["divisions"], json!(2));
    assert_eq!(s["divisionsWithLeader"], json!(2));
    let active: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM users u WHERE u.status = 1 AND EXISTS (
                 SELECT 1 FROM "userCompany" uc WHERE uc."userId" = u.id AND uc."companyId" = {} AND uc.status = 1)"#,
            fx.id
        ))
        .await;
    let assigned: i64 = app
        .scalar(&format!(
            r#"SELECT count(DISTINCT "userId") FROM "divisionMember" WHERE "companyId" = {} AND "deletedAt" IS NULL"#,
            fx.id
        ))
        .await;
    assert_eq!(s["activeEmployees"], json!(active));
    assert_eq!(s["assignedEmployees"], json!(assigned));
    assert_eq!(s["unassignedEmployees"], json!(active - assigned));
    assert_eq!(r.json["meta"]["timeZone"], json!(TZ));
    assert!(r.json["meta"]["today"].as_str().unwrap().len() == 10);
    assert_key_order(&r.text(), &["data", "summary", "meta"]);
    assert_key_order(
        &r.text(),
        &[
            "uuid",
            "name",
            "description",
            "version",
            "leader",
            "leaders",
            "memberCount",
            "membersPreview",
            "trackedTodaySeconds",
            "trackedWeekSeconds",
        ],
    );

    // Manager (company-wide): sama dengan admin.
    let r = app
        .get("/v1/divisions")
        .token(&fx.manager.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["data"].as_array().unwrap().len(), 2);
    assert!(r.json["summary"].is_object());

    // Leader (STAFF): hanya divisi yang dipimpin, tanpa ringkasan.
    let r = app.get("/v1/divisions").token(&fx.staff.token).send().await;
    r.assert_status(200);
    assert_eq!(r.json["data"].as_array().unwrap().len(), 1, "{}", r.text());
    assert_eq!(r.json["data"][0]["uuid"], json!(c.a_uuid));
    assert_eq!(r.json["summary"], Value::Null);
    // `search` berlaku di antara divisi itu.
    let r = app
        .get("/v1/divisions?search=zzz")
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["data"].as_array().unwrap().len(), 0);

    // Member biasa (SELF): 403 DIVISION_NOT_LEADER.
    let r = app
        .get("/v1/divisions")
        .token(&fx.staff2.token)
        .send()
        .await;
    assert_code(&r, 403, "DIVISION_NOT_LEADER");
    assert_eq!(
        r.json["message"],
        json!("Only admins, managers and division Leaders can view divisions")
    );

    // Tanpa token → 401.
    app.get("/v1/divisions").send().await.assert_status(401);
}

#[tokio::test]
async fn list_search_escapes_like_wildcards_and_validates() {
    let Some(c) = setup().await else { return };
    let (app, fx) = (&c.app, &c.fx);
    // Case-insensitive substring.
    let r = app
        .get("/v1/divisions?search=%20IT-a%20")
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["data"].as_array().unwrap().len(), 1);
    // `_` dan `%` literal, bukan wildcard.
    for q in ["_", "%25", "i_-A"] {
        let r = app
            .get(&format!("/v1/divisions?search={q}"))
            .token(&fx.admin.token)
            .send()
            .await;
        r.assert_status(200);
        assert_eq!(r.json["data"].as_array().unwrap().len(), 0, "search={q}");
    }
    // Ringkasan company tidak dipengaruhi `search`.
    let r = app
        .get("/v1/divisions?search=it-a")
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(r.json["summary"]["divisions"], json!(2));
    // Validasi.
    let long = "x".repeat(61);
    let r = app
        .get(&format!("/v1/divisions?search={long}"))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!(["search must be shorter than or equal to 60 characters"])
    );
}

#[tokio::test]
async fn employees_list_roles_and_static_path_shadowing() {
    let Some(c) = setup().await else { return };
    let (app, fx) = (&c.app, &c.fx);

    let r = app
        .get("/v1/divisions/employees?itemsPerPage=500")
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let data = r.json["data"].as_array().unwrap();
    let total = r.json["meta"]["total"].as_i64().unwrap();
    assert_eq!(data.len() as i64, total.min(500));
    let staff = data
        .iter()
        .find(|e| e["uuid"] == json!(fx.staff.uuid))
        .expect("staff ada di daftar");
    assert_eq!(staff["email"], json!(fx.staff.username));
    assert_eq!(staff["division"]["name"], json!("it-A"));
    assert_eq!(staff["division"]["role"], json!("LEADER"));
    assert_eq!(staff["divisions"].as_array().unwrap().len(), 1);
    assert_eq!(r.json["meta"]["pageNumber"], json!(1));
    assert_eq!(r.json["meta"]["itemsPerPage"], json!(500));
    assert_key_order(&r.text(), &["data", "meta"]);

    // Paging + pencarian.
    let r = app
        .get("/v1/divisions/employees?pageNumber=2&itemsPerPage=1")
        .token(&fx.manager.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["data"].as_array().unwrap().len(), 1);
    assert_eq!(r.json["meta"]["pageNumber"], json!(2));
    assert_eq!(r.json["meta"]["itemsPerPage"], json!(1));
    let name: String = app
        .scalar(&format!(
            "SELECT name FROM users WHERE id = {}",
            fx.staff.id
        ))
        .await;
    let r = app
        .get(&format!(
            "/v1/divisions/employees?search={}",
            name.replace(' ', "%20").to_uppercase()
        ))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert!(
        r.json["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["uuid"] == json!(fx.staff.uuid))
    );

    // Validasi query: urutan pesan class-validator.
    let r = app
        .get("/v1/divisions/employees?pageNumber=0&itemsPerPage=501")
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "pageNumber must not be less than 1",
            "itemsPerPage must not be greater than 500"
        ])
    );

    // VIEW_ROLES: STAFF dan Leader ditolak oleh RolesGuard (401 `Access denied`, seperti Node).
    let r = app
        .get("/v1/divisions/employees")
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_status(401);
    assert_eq!(
        r.json["message"],
        json!("Access denied: insufficient permissions")
    );

    // Method lain pada path statis jatuh ke `:uuid` (ParseUUIDPipe) → 400.
    for (m, who) in [("PATCH", &fx.admin), ("DELETE", &fx.admin)] {
        for path in ["employees", "my-scope"] {
            let r = app
                .request(m.parse().unwrap(), &format!("/v1/divisions/{path}"))
                .token(&who.token)
                .json(&json!({ "version": 0 }))
                .send()
                .await;
            r.assert_status(400);
            assert_eq!(
                r.json["message"],
                json!("Validation failed (uuid is expected)"),
                "{m} {path}"
            );
        }
    }
    // Role yang tak boleh menulis ditolak sebelum pipe (guard lebih dulu).
    app.request("PATCH".parse().unwrap(), "/v1/divisions/employees")
        .token(&fx.manager.token)
        .json(&json!({ "version": 0 }))
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
async fn my_scope_company_division_self() {
    let Some(c) = setup().await else { return };
    let (app, fx) = (&c.app, &c.fx);

    for who in [&fx.admin, &fx.manager] {
        let r = app
            .get("/v1/divisions/my-scope")
            .token(&who.token)
            .send()
            .await;
        r.assert_status(200);
        assert_eq!(r.json, json!({ "data": { "kind": "COMPANY" } }));
    }
    let r = app
        .get("/v1/divisions/my-scope")
        .token(&fx.staff2.token)
        .send()
        .await;
    assert_eq!(r.json, json!({ "data": { "kind": "SELF" } }));

    let r = app
        .get("/v1/divisions/my-scope")
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let d = &r.json["data"];
    assert_eq!(d["kind"], json!("DIVISION"));
    assert_eq!(d["division"], json!({ "uuid": c.a_uuid, "name": "it-A" }));
    assert_eq!(
        d["divisions"],
        json!([{ "uuid": c.a_uuid, "name": "it-A" }])
    );
    // Pemanggil dulu, Leader lain, lalu Member (urut nama).
    let emp = d["employees"].as_array().unwrap();
    assert_eq!(emp.len(), 3);
    assert_eq!(emp[0]["id"], json!(fx.staff.id));
    assert_eq!(emp[0]["role"], json!("LEADER"));
    assert!(emp[1..].iter().all(|e| e["role"] == json!("MEMBER")));

    // Jangkauan identik dengan resolver milik mz-trackings (guard anti-penyimpangan).
    let mine = mz_divisions::resolve_viewer_scope(app.db(), fx.staff.id, fx.id)
        .await
        .unwrap();
    let theirs = mz_trackings::division::resolve_viewer_scope(app.db(), fx.staff.id, fx.id)
        .await
        .unwrap();
    let (
        mz_divisions::Scope::Division { user_ids: a, .. },
        mz_trackings::division::ViewerScope::Division { user_ids: b },
    ) = (mine, theirs)
    else {
        panic!("keduanya harus DIVISION");
    };
    assert_eq!(a, b);
    assert_eq!(
        ids(&json!(
            emp.iter().map(|e| e["id"].clone()).collect::<Vec<_>>()
        ))
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>(),
        a.iter().map(|i| i64::from(*i)).collect()
    );
}

#[tokio::test]
async fn detail_access_rules() {
    let Some(c) = setup().await else { return };
    let (app, fx) = (&c.app, &c.fx);

    let r = app
        .get(&format!("/v1/divisions/{}", c.a_uuid))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let d = &r.json["data"];
    assert_eq!(d["uuid"], json!(c.a_uuid));
    assert_eq!(d["name"], json!("it-A"));
    assert_eq!(d["description"], Value::Null);
    assert_eq!(d["version"], json!(0));
    assert_eq!(d["viewer"], json!({ "canManage": true }));
    let members = d["members"].as_array().unwrap();
    assert_eq!(members.len(), 3);
    // Leader dulu.
    assert_eq!(members[0]["uuid"], json!(fx.staff.uuid));
    assert_eq!(members[0]["role"], json!("LEADER"));
    assert_eq!(members[0]["email"], json!(fx.staff.username));
    let joined = members[0]["joinedAt"].as_str().unwrap();
    assert!(joined.ends_with('Z') && joined.len() == 24, "{joined}");
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
            "trackedTodaySeconds",
            "trackedWeekSeconds",
            "viewer",
        ],
    );
    assert_key_order(&r.text(), &["uuid", "name", "email", "role", "joinedAt"]);

    // Manager: boleh baca, tidak boleh mengelola (Q1 = B). Leader: sama.
    let r = app
        .get(&format!("/v1/divisions/{}", c.a_uuid))
        .token(&fx.manager.token)
        .send()
        .await;
    assert_eq!(r.json["data"]["viewer"], json!({ "canManage": false }));
    let r = app
        .get(&format!("/v1/divisions/{}", c.a_uuid))
        .token(&fx.staff.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["viewer"], json!({ "canManage": false }));

    // Leader membuka divisi lain: 403 tanpa cek keberadaan (uuid asing pun 403, bukan 404).
    let msg = "You can only open a division you lead";
    for u in [
        c.b_uuid.as_str(),
        "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
        "12345678-1234-1234-1234-123456789012",
    ] {
        let r = app
            .get(&format!("/v1/divisions/{u}"))
            .token(&fx.staff.token)
            .send()
            .await;
        assert_code(&r, 403, "DIVISION_SCOPE_FORBIDDEN");
        assert_eq!(r.json["message"], json!(msg));
    }
    // Member biasa: DIVISION_NOT_LEADER.
    let r = app
        .get(&format!("/v1/divisions/{}", c.a_uuid))
        .token(&fx.staff2.token)
        .send()
        .await;
    assert_code(&r, 403, "DIVISION_NOT_LEADER");

    // Admin: tak ada → 404; bukan UUID (bentuk) → 400 pipe; bentuk valid tapi bukan isUUID → 404.
    let r = app
        .get("/v1/divisions/3f2504e0-4f89-41d3-9a0c-0305e82c3301")
        .token(&fx.admin.token)
        .send()
        .await;
    assert_code(&r, 404, "DIVISION_NOT_FOUND");
    assert_eq!(r.json["message"], json!("Division not found"));
    let r = app
        .get("/v1/divisions/not-a-uuid")
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!("Validation failed (uuid is expected)")
    );
    let r = app
        .get("/v1/divisions/12345678-1234-1234-1234-123456789012")
        .token(&fx.admin.token)
        .send()
        .await;
    assert_code(&r, 404, "DIVISION_NOT_FOUND");

    // Divisi terhapus → 404.
    app.exec(&format!(
        r#"UPDATE division SET "deletedAt" = NOW() WHERE uuid = '{}'"#,
        c.b_uuid
    ))
    .await;
    let r = app
        .get(&format!("/v1/divisions/{}", c.b_uuid))
        .token(&fx.admin.token)
        .send()
        .await;
    assert_code(&r, 404, "DIVISION_NOT_FOUND");
}

#[tokio::test]
async fn gates_pilot_plan_and_module() {
    let Some(c) = setup().await else { return };
    let (app, fx) = (&c.app, &c.fx);
    let cid = fx.id;

    // Saklar pilot tertutup: 403 MODULE_DISABLED (jangkauan Leader pun SELF).
    app.exec(&format!(
        r#"UPDATE company SET "divisionsEnabled" = false WHERE id = {cid}"#
    ))
    .await;
    for path in [
        "/v1/divisions",
        "/v1/divisions/my-scope",
        "/v1/divisions/employees",
    ] {
        let r = app.get(path).token(&fx.admin.token).send().await;
        assert_code(&r, 403, "MODULE_DISABLED");
        assert_eq!(
            r.json["message"],
            json!("Divisions is not enabled for this company")
        );
    }
    app.exec(&format!(
        r#"UPDATE company SET "divisionsEnabled" = true WHERE id = {cid}"#
    ))
    .await;

    // Paket tanpa fitur divisi → ditolak gerbang paket sebelum handler.
    app.exec(&format!(
        r#"UPDATE "companyPlanOverride" SET divisions = false WHERE "companyId" = {cid}"#
    ))
    .await;
    let r = app.get("/v1/divisions").token(&fx.admin.token).send().await;
    r.assert_status(403);
    app.exec(&format!(
        r#"UPDATE "companyPlanOverride" SET divisions = true WHERE "companyId" = {cid}"#
    ))
    .await;

    // Modul TIME_TRACKING mati → ditolak.
    app.exec(&format!(
        r#"UPDATE company SET "moduleTimeTrackingEnabled" = false WHERE id = {cid}"#
    ))
    .await;
    // Modul fitur di-cache in-process (60 dtk): invalidasi seperti endpoint admin company.
    mz_core::feature_modules::invalidate_company(app.state(), cid);
    app.get("/v1/divisions")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(403);
    app.exec(&format!(
        r#"UPDATE company SET "moduleTimeTrackingEnabled" = true WHERE id = {cid}"#
    ))
    .await;
    mz_core::feature_modules::invalidate_company(app.state(), cid);
    app.get("/v1/divisions")
        .token(&fx.admin.token)
        .send()
        .await
        .assert_status(200);
}

/// Seed timeline: bersihkan data 10 hari terakhir user fixture, lalu isi data deterministik.
async fn seed_time(c: &Ctx) -> (String, String) {
    let (app, fx) = (&c.app, &c.fx);
    let users = [fx.staff.id, fx.staff2.id, fx.admin.id]
        .map(|i| i.to_string())
        .join(",");
    let pid = c.project_id;
    app.exec(&format!(
        r#"UPDATE project SET billable = true WHERE id = {pid};
           UPDATE tracking SET status = 0 WHERE "userId" IN ({users}) AND "takenDate" >= current_date - 600;"#
    ))
    .await;
    // d1 = hari ke-3, d2 = hari ke-2 (tanggal UTC, di dalam batas riwayat 3 bulan).
    let (d1, d2): (String, String) =
        sqlx::query_as("SELECT (current_date - 3)::text, (current_date - 2)::text")
            .fetch_one(app.db())
            .await
            .unwrap();
    let add = |user: i32, date: &str, s: &str, e: &str, bill: &str, note: &str| {
        format!(
            r#"INSERT INTO tracking ("takenDate", "userId", status, "isAbsent")
                 VALUES ('{date}', {user}, 1, false)
                 ON CONFLICT ("userId", "takenDate") DO UPDATE SET status = 1;
               INSERT INTO "trackingTimeline" (note, "startAt", "endAt", "startAtUtc", "endAtUtc", timezone,
                                               "trackingId", "projectId", status, billable)
                 SELECT '{note}', '{date} {s}'::timestamp, '{date} {e}'::timestamp,
                        '{date} {s}'::timestamp, '{date} {e}'::timestamp, 'UTC',
                        t.id, {pid}, 1, {bill}
                 FROM tracking t WHERE t."userId" = {user} AND t."takenDate" = '{date}';"#
        )
    };
    let sql = [
        // staff: A 1 jam (billable NULL → ikut project = true) + B 30 menit (billable = false).
        add(fx.staff.id, &d1, "09:00:00", "10:00:00", "NULL", "itdiv A"),
        add(fx.staff.id, &d1, "13:00:00", "13:30:00", "false", "itdiv B"),
        // staff2: 10 menit + (hari berikutnya) 1,999 detik → dibulatkan ke bawah = 1 detik.
        add(fx.staff2.id, &d1, "09:00:00", "09:10:00", "NULL", "itdiv C"),
        add(
            fx.staff2.id,
            &d2,
            "10:00:00",
            "10:00:01.999",
            "true",
            "itdiv D",
        ),
    ]
    .join("\n");
    app.exec(&sql).await;
    (d1, d2)
}

#[tokio::test]
async fn report_totals_billable_rounding_and_range_rules() {
    let Some(c) = setup().await else { return };
    let (app, fx) = (&c.app, &c.fx);
    let (d1, d2) = seed_time(&c).await;
    let url = format!(
        "/v1/divisions/{}/report?startAt={d1}T00:00:00.000Z&endAt={d2}T23:59:59.999Z",
        c.a_uuid
    );

    let r = app.get(&url).token(&fx.admin.token).send().await;
    r.assert_status(200).assert_no_sql_error();
    let d = &r.json["data"];
    assert_eq!(
        d["range"],
        json!({ "startAt": d1, "endAt": d2, "clampedByHistoryLimit": false })
    );
    let m = |uuid: &str| {
        d["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["uuid"] == json!(uuid))
            .unwrap()
            .clone()
    };
    assert_eq!(
        m(&fx.staff.uuid),
        json!({ "uuid": fx.staff.uuid, "name": m(&fx.staff.uuid)["name"],
                "totalSeconds": 5400, "billableSeconds": 3600, "nonBillableSeconds": 1800 })
    );
    assert_eq!(m(&fx.staff2.uuid)["totalSeconds"], json!(601));
    assert_eq!(m(&fx.staff2.uuid)["billableSeconds"], json!(601));
    // Anggota tanpa data: nol.
    assert_eq!(m(&fx.admin.uuid)["totalSeconds"], json!(0));
    // 6001 / 3 anggota = 2000,33 → 2000.
    assert_eq!(
        d["totals"],
        json!({ "totalSeconds": 6001, "billableSeconds": 4201,
                "nonBillableSeconds": 1800, "averagePerMemberSeconds": 2000 })
    );
    assert_key_order(&r.text(), &["range", "totals", "members"]);
    assert_key_order(
        &r.text(),
        &[
            "totalSeconds",
            "billableSeconds",
            "nonBillableSeconds",
            "averagePerMemberSeconds",
        ],
    );

    // Leader boleh (divisinya), Leader divisi lain tidak.
    app.get(&url)
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(200);
    let other = url.replace(&c.a_uuid, &c.b_uuid);
    assert_code(
        &app.get(&other).token(&fx.staff.token).send().await,
        403,
        "DIVISION_SCOPE_FORBIDDEN",
    );
    assert_code(
        &app.get(&url).token(&fx.staff2.token).send().await,
        403,
        "DIVISION_NOT_LEADER",
    );

    // Satu hari saja (d2): hanya timeline 1,999 detik.
    let r = app
        .get(&format!(
            "/v1/divisions/{}/report?startAt={d2}&endAt={d2}",
            c.a_uuid
        ))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["totals"]["totalSeconds"], json!(1));
    assert_eq!(
        r.json["data"]["totals"]["averagePerMemberSeconds"],
        json!(0)
    );

    // Rentang lebih tua dari batas riwayat (3 bulan): digeser; seluruhnya lebih tua → satu hari.
    let r = app
        .get(&format!(
            "/v1/divisions/{}/report?startAt=2020-01-01T00:00:00.000Z&endAt={d2}T00:00:00.000Z",
            c.a_uuid
        ))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(400); // > 366 hari
    let (old_from, old_to): (String, String) =
        sqlx::query_as("SELECT (current_date - 300)::text, (current_date - 290)::text")
            .fetch_one(app.db())
            .await
            .unwrap();
    let r = app
        .get(&format!(
            "/v1/divisions/{}/report?startAt={old_from}&endAt={old_to}",
            c.a_uuid
        ))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    let range = &r.json["data"]["range"];
    assert_eq!(range["clampedByHistoryLimit"], json!(true));
    assert_eq!(range["startAt"], range["endAt"]);
    assert_eq!(r.json["data"]["totals"]["totalSeconds"], json!(0));
    let (lo, hi): (String, String) = sqlx::query_as(
        "SELECT (current_date - interval '3 months' - interval '3 days')::date::text, \
                (current_date - interval '3 months' + interval '3 days')::date::text",
    )
    .fetch_one(app.db())
    .await
    .unwrap();
    let start = range["startAt"].as_str().unwrap();
    assert!(
        lo.as_str() <= start && start <= hi.as_str(),
        "{start} {lo} {hi}"
    );
    // Mulai lama, selesai baru: hanya `startAt` yang digeser.
    let r = app
        .get(&format!(
            "/v1/divisions/{}/report?startAt={old_from}&endAt={d2}",
            c.a_uuid
        ))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(
        r.json["data"]["range"]["clampedByHistoryLimit"],
        json!(true)
    );
    assert_eq!(r.json["data"]["range"]["endAt"], json!(d2));
    assert_eq!(r.json["data"]["totals"]["totalSeconds"], json!(6001));

    // Tanpa parameter: awal pekan company s.d. hari ini (zona Asia/Jakarta).
    let (wsd,): (i32,) = sqlx::query_as(r#"SELECT "workweekStartDay" FROM company WHERE id = $1"#)
        .bind(fx.id)
        .fetch_one(app.db())
        .await
        .unwrap();
    let now_ms = chrono_now_ms();
    let (today, week_start) = mz_divisions::company_day_and_week(now_ms, Some(TZ), wsd);
    let r = app
        .get(&format!("/v1/divisions/{}/report", c.a_uuid))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["data"]["range"]["endAt"], json!(today));
    assert_eq!(r.json["data"]["range"]["startAt"], json!(week_start));
}

fn chrono_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap())
        .unwrap()
}

#[tokio::test]
async fn report_query_validation_messages() {
    let Some(c) = setup().await else { return };
    let (app, fx) = (&c.app, &c.fx);
    let base = format!("/v1/divisions/{}/report", c.a_uuid);
    let get = |q: &str| {
        app.get(&format!("{base}?{q}"))
            .token(&fx.admin.token)
            .send()
    };

    let r = get("startAt=2026-11-01").await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "startAt and endAt must be given together",
            "endAt must be a valid ISO 8601 date string"
        ])
    );
    let r = get("endAt=2026-11-01").await;
    assert_eq!(
        r.json["message"],
        json!(["startAt and endAt must be given together"])
    );
    let r = get("startAt=2026-11-02&endAt=2026-11-01").await;
    assert_eq!(
        r.json["message"],
        json!(["endAt must be on or after startAt"])
    );
    let r = get("startAt=foo&endAt=2026-11-01").await;
    assert_eq!(
        r.json["message"],
        json!([
            "startAt must be a valid ISO 8601 date string",
            "startAt and endAt must be valid dates"
        ])
    );
    let r = get("startAt=2025-01-01&endAt=2026-11-01").await;
    assert_eq!(
        r.json["message"],
        json!(["Date range cannot exceed 366 days"])
    );
    // Uuid divisi diperiksa sebelum query.
    let r = app
        .get("/v1/divisions/nope/report?startAt=x")
        .token(&fx.admin.token)
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!("Validation failed (uuid is expected)")
    );
}

#[tokio::test]
async fn tracking_status_windows_and_project() {
    let Some(c) = setup().await else { return };
    let (app, fx) = (&c.app, &c.fx);
    seed_time(&c).await;
    let wsd = 1;
    let (today, _) = mz_divisions::company_day_and_week(chrono_now_ms(), Some(TZ), wsd);
    let pid = c.project_id;
    // staff2: berakhir 2 menit lalu (ONLINE, 30 menit tracked); staff: 30 menit lalu (IDLE);
    // admin: tidak ada aktivitas (OFFLINE). Detik dibulatkan supaya durasi tepat 1800.
    let sql = |user: i32, start_ago: &str, end_ago: &str, note: &str| {
        format!(
            r#"INSERT INTO tracking ("takenDate", "userId", status, "isAbsent")
                 VALUES ('{today}', {user}, 1, false)
                 ON CONFLICT ("userId", "takenDate") DO UPDATE SET status = 1;
               INSERT INTO "trackingTimeline" (note, "startAt", "endAt", "startAtUtc", "endAtUtc", timezone,
                                               "trackingId", "projectId", status)
                 SELECT '{note}',
                        date_trunc('second', now() at time zone 'utc') - interval '{start_ago}',
                        date_trunc('second', now() at time zone 'utc') - interval '{end_ago}',
                        date_trunc('second', now() at time zone 'utc') - interval '{start_ago}',
                        date_trunc('second', now() at time zone 'utc') - interval '{end_ago}',
                        'UTC', t.id, {pid}, 1
                 FROM tracking t WHERE t."userId" = {user} AND t."takenDate" = '{today}';"#
        )
    };
    app.exec(&format!(
        "{}\n{}",
        sql(fx.staff2.id, "32 minutes", "2 minutes", "itdiv live"),
        sql(fx.staff.id, "60 minutes", "30 minutes", "itdiv idle")
    ))
    .await;

    let r = app
        .get(&format!("/v1/divisions/{}/tracking", c.a_uuid))
        .token(&fx.admin.token)
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    let d = r.json["data"].as_array().unwrap();
    assert_eq!(d.len(), 3);
    let of = |uuid: &str| d.iter().find(|x| x["uuid"] == json!(uuid)).unwrap().clone();
    let live = of(&fx.staff2.uuid);
    assert_eq!(live["status"], json!("ONLINE"));
    assert_eq!(live["role"], json!("MEMBER"));
    assert_eq!(live["trackedTodaySeconds"], json!(1800));
    assert_eq!(live["currentProject"]["uuid"], json!(c.project_uuid));
    assert!(live["lastActivityAt"].as_str().unwrap().ends_with('Z'));
    let idle = of(&fx.staff.uuid);
    assert_eq!(idle["status"], json!("IDLE"));
    assert_eq!(idle["role"], json!("LEADER"));
    assert!(idle["currentProject"].is_object());
    let off = of(&fx.admin.uuid);
    assert_eq!(off["status"], json!("OFFLINE"));
    assert_eq!(off["currentProject"], Value::Null);
    assert_eq!(off["lastActivityAt"], Value::Null);
    assert_eq!(off["trackedTodaySeconds"], json!(0));
    // Leader dulu.
    assert_eq!(d[0]["role"], json!("LEADER"));
    assert_eq!(
        r.json["meta"],
        json!({ "timeZone": TZ, "today": today, "onlineWindowMinutes": 10, "idleWindowMinutes": 60 })
    );
    assert_key_order(
        &r.text(),
        &[
            "uuid",
            "name",
            "role",
            "status",
            "currentProject",
            "lastActivityAt",
            "trackedTodaySeconds",
        ],
    );

    // Leader boleh; Member / divisi lain tidak.
    app.get(&format!("/v1/divisions/{}/tracking", c.a_uuid))
        .token(&fx.staff.token)
        .send()
        .await
        .assert_status(200);
    assert_code(
        &app.get(&format!("/v1/divisions/{}/tracking", c.b_uuid))
            .token(&fx.staff.token)
            .send()
            .await,
        403,
        "DIVISION_SCOPE_FORBIDDEN",
    );
    assert_code(
        &app.get(&format!("/v1/divisions/{}/tracking", c.a_uuid))
            .token(&fx.staff2.token)
            .send()
            .await,
        403,
        "DIVISION_NOT_LEADER",
    );
}
