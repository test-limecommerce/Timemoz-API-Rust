//! Uji integrasi sisi baca/restore/retensi audit log terhadap Postgres NYATA.
//! Jalankan (lihat `crates/mz-testkit/README.md`):
//! `MZ_TEST_DATABASE_URL=… cargo test -p mz-audit --test it_audit_admin`
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicU32, Ordering};

use axum::Router;
use chrono::{Duration, Utc};
use mz_core::AppState;
use mz_testkit::{Company, TestApp, TestResponse};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::RwLock;

const MISSING: &str = "00000000-0000-4000-8000-00000000dead";
const OTHER_COMPANY: i32 = 987_654;
const BASE: &str = "/v1/admin/audit-logs";

static SEQ: AtomicU32 = AtomicU32::new(0);
/// Test biasa memegang read lock; test gerbang paket (mengubah override) memegang write lock.
static PLAN: RwLock<()> = RwLock::const_new(());

fn uniq(prefix: &str) -> String {
    format!(
        "{prefix}{}x{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

fn build(state: &AppState) -> Router {
    mz_audit::router(state)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

async fn open_plan(app: &TestApp, company: i32, allow: bool) {
    sqlx::query(
        r#"INSERT INTO "companyPlanOverride" ("companyId", "auditLog", "updatedAt")
           VALUES ($1, $2, now())
           ON CONFLICT ("companyId") DO UPDATE SET "auditLog" = EXCLUDED."auditLog""#,
    )
    .bind(company)
    .bind(allow)
    .execute(app.db())
    .await
    .unwrap();
}

/// Aplikasi + fixture, dengan fitur audit-log paket terbuka. `None` = dilewati (tanpa DB).
async fn setup() -> Option<(TestApp, Company)> {
    // Galat 500 hanya tercatat lewat `tracing::error!`; tampil di output test yang gagal.
    let _ = tracing_subscriber::fmt()
        .with_env_filter("error")
        .with_test_writer()
        .try_init();
    let app = TestApp::new(build).await?;
    let fx = app.company().await;
    open_plan(&app, fx.id, true).await;
    Some((app, fx))
}

/// Satu baris `auditLog` seed.
#[derive(Clone)]
struct Log {
    company: i32,
    entity_type: &'static str,
    entity_id: i32,
    entity_uuid: Option<String>,
    label: String,
    action: &'static str,
    actor: Option<(i32, &'static str)>,
    actor_type: &'static str,
    before: Option<Value>,
    after: Option<Value>,
    changed: Vec<String>,
    restorable: bool,
    age_days: f64,
}

impl Log {
    fn new(company: i32, entity_type: &'static str, label: &str, action: &'static str) -> Self {
        Self {
            company,
            entity_type,
            entity_id: 1,
            entity_uuid: Some(MISSING.to_owned()),
            label: label.to_owned(),
            action,
            actor: None,
            actor_type: "user",
            before: None,
            after: None,
            changed: Vec::new(),
            restorable: false,
            age_days: 0.0,
        }
    }

    async fn insert(&self, db: &PgPool) -> String {
        sqlx::query_scalar(
            r#"INSERT INTO "auditLog"
                 ("companyId", "entityType", "entityId", "entityUuid", "entityLabel", action, "actorType",
                  "actorId", "actorName", "before", "after", "changedFields", "isRestorable", "createdAt")
               VALUES ($1, $2::text::"AuditEntityType", $3, $4::uuid, $5, $6::text::"AuditAction",
                       $7::text::"AuditActorType", $8, $9, $10, $11, $12, $13,
                       (now() AT TIME ZONE 'utc') - ($14::float8 * interval '1 day'))
               RETURNING uuid::text"#,
        )
        .bind(self.company)
        .bind(self.entity_type)
        .bind(self.entity_id)
        .bind(&self.entity_uuid)
        .bind(&self.label)
        .bind(self.action)
        .bind(self.actor_type)
        .bind(self.actor.map(|a| a.0))
        .bind(self.actor.map(|a| a.1))
        .bind(&self.before)
        .bind(&self.after)
        .bind(&self.changed)
        .bind(self.restorable)
        .bind(self.age_days)
        .fetch_one(db)
        .await
        .unwrap()
    }
}

fn ids(res: &TestResponse) -> Vec<String> {
    res.json["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

fn total(res: &TestResponse) -> i64 {
    res.json["meta"]["total"].as_i64().unwrap()
}

// ── list / filter ───────────────────────────────────────────────────────────

#[tokio::test]
async fn list_filters_pagination_and_shape() {
    let _g = PLAN.read().await;
    let Some((app, fx)) = setup().await else {
        return;
    };
    let p = uniq("itaudl");
    let admin = (fx.admin.id, "Budi Santoso");
    let manager = (fx.manager.id, "Sari Manager");
    let mk = |et: &'static str, action: &'static str, actor, age: f64| Log {
        actor,
        age_days: age,
        ..Log::new(fx.id, et, &format!("{p} item"), action)
    };
    let a = Log {
        changed: vec!["address".into()],
        ..mk("store", "update", Some(admin), 1.0)
    }
    .insert(app.db())
    .await;
    let b = mk("store", "delete", Some(admin), 2.0)
        .insert(app.db())
        .await;
    let c = mk("geofence", "create", Some(manager), 3.0)
        .insert(app.db())
        .await;
    let d = Log {
        actor_type: "system",
        ..mk("employee", "update", None, 4.0)
    }
    .insert(app.db())
    .await;
    let e = mk("store", "create", Some(admin), 5.0)
        .insert(app.db())
        .await;
    // Company lain tidak boleh terlihat.
    Log::new(OTHER_COMPANY, "store", &format!("{p} item"), "update")
        .insert(app.db())
        .await;

    let get = |q: String| {
        let app = &app;
        let uid = fx.admin.id;
        async move {
            let r = app.get(&format!("{BASE}?{q}")).user(uid).send().await;
            r.assert_status(200);
            r
        }
    };

    // Urut createdAt desc; company lain terbuang; search tak peka huruf besar/kecil.
    let r = get(format!("search={p}")).await;
    assert_eq!(
        ids(&r),
        vec![a.clone(), b.clone(), c.clone(), d.clone(), e.clone()]
    );
    assert_eq!(
        r.json["meta"],
        json!({"total": 5, "pageNumber": 1, "itemsPerPage": 20, "totalPages": 1})
    );
    assert_eq!(total(&get(format!("search={}", p.to_uppercase())).await), 5);

    // Bentuk baris §6.1 (tanpa before/after).
    let row = &r.json["data"][0];
    assert_eq!(row["id"], a);
    assert_eq!(row["entityType"], "store");
    assert_eq!(
        row["entity"],
        json!({"id": MISSING, "label": format!("{p} item")})
    );
    assert_eq!(row["subEntity"], Value::Null);
    assert_eq!(row["action"], "update");
    assert_eq!(row["actionLabel"], "Updated");
    assert_eq!(
        row["actor"],
        json!({"id": fx.admin.uuid, "name": "Budi Santoso", "role": null, "type": "user"})
    );
    assert_eq!(row["changedFields"], json!(["address"]));
    assert_eq!(row["isRestorable"], false);
    assert_eq!(row["restoredAt"], Value::Null);
    assert!(row["createdAt"].is_i64());
    assert!(row.get("before").is_none() && row.get("after").is_none());
    // Aksi `delete` oleh system dibaca "Deactivated by system".
    let sys = Log {
        actor_type: "system",
        ..Log::new(fx.id, "store", &format!("{p} sys"), "delete")
    }
    .insert(app.db())
    .await;
    let r = get(format!("search={p}+sys")).await;
    assert_eq!(ids(&r), vec![sys.clone()]);
    assert_eq!(r.json["data"][0]["actionLabel"], "Deactivated by system");
    assert_eq!(r.json["data"][0]["actor"]["id"], Value::Null);

    // Filter.
    assert_eq!(total(&get(format!("search={p}&entityType=store")).await), 4);
    assert_eq!(
        total(&get(format!("search={p}&entityType=store,geofence")).await),
        5
    );
    assert_eq!(
        total(&get(format!("search={p}&entityType[]=geofence")).await),
        1
    );
    assert_eq!(
        total(
            &get(format!(
                "search={p}&entityType=geofence&entityType=employee"
            ))
            .await
        ),
        2
    );
    assert_eq!(total(&get(format!("search={p}&action=create")).await), 2);
    assert_eq!(
        total(&get(format!("search={p}&action=update&action=delete")).await),
        4
    );
    let r = get(format!("search={p}&actorId={}", fx.admin.uuid)).await;
    assert_eq!(ids(&r), vec![a.clone(), b.clone(), e.clone()]);
    let r = get(format!(
        "search={p}&actorId={},{}",
        fx.admin.uuid, fx.manager.uuid
    ))
    .await;
    assert_eq!(total(&r), 4);
    // Semua uuid pelaku tak dikenal → daftar kosong, bukan filter yang diabaikan.
    let r = get(format!("search={p}&actorId={MISSING}")).await;
    assert_eq!(
        r.json,
        json!({"data": [], "meta": {"total": 0, "pageNumber": 1, "itemsPerPage": 20, "totalPages": 0}})
    );
    // Nama pelaku ikut dicari.
    assert!(total(&get("search=Budi+Santoso".to_owned()).await) >= 3);
    // Rentang tanggal: antara 3,5 dan 1,5 hari lalu → b, c.
    let iso = |days: f64| {
        (Utc::now() - Duration::milliseconds((days * 86_400_000.0) as i64))
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string()
    };
    let r = get(format!(
        "search={p}&dateFrom={}&dateTo={}",
        iso(3.5),
        iso(1.5)
    ))
    .await;
    assert_eq!(ids(&r), vec![b.clone(), c.clone()]);
    // Hanya batas bawah.
    let r = get(format!("search={p}&dateFrom={}", iso(1.5))).await;
    assert_eq!(ids(&r), vec![sys.clone(), a.clone()]);
    // entityUuid.
    assert_eq!(
        total(&get(format!("search={p}&entityUuid={MISSING}")).await),
        6
    );

    // Paginasi: urutan sys, a, b, c, d, e.
    let r = get(format!("search={p}&itemsPerPage=2&pageNumber=2")).await;
    assert_eq!(ids(&r), vec![b.clone(), c.clone()]);
    assert_eq!(
        r.json["meta"],
        json!({"total": 6, "pageNumber": 2, "itemsPerPage": 2, "totalPages": 3})
    );
    let r = get(format!("search={p}&itemsPerPage=2&pageNumber=9")).await;
    assert!(ids(&r).is_empty());
    assert_eq!(total(&r), 6);

    // Validasi → 400 ala ValidationPipe.
    let bad = app
        .get(&format!(
            "{BASE}?entityType=bogus&pageNumber=0&itemsPerPage=101&dateFrom=x"
        ))
        .user(fx.admin.id)
        .send()
        .await;
    bad.assert_status(400);
    assert_eq!(bad.json["error"], "Bad Request");
    assert_eq!(bad.json["statusCode"], 400);
    let msgs: Vec<&str> = bad.json["message"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect();
    assert_eq!(msgs.len(), 4);
    assert!(msgs[0].starts_with(
        "each value in entityType must be one of the following values: company, employee,"
    ));
    assert_eq!(msgs[1], "dateFrom must be a valid ISO 8601 date string");
    assert_eq!(msgs[2], "pageNumber must not be less than 1");
    assert_eq!(msgs[3], "itemsPerPage must not be greater than 100");
}

// ── detail / riwayat entitas / peran ────────────────────────────────────────

#[tokio::test]
async fn detail_entity_history_and_role_split() {
    let _g = PLAN.read().await;
    let Some((app, fx)) = setup().await else {
        return;
    };
    let p = uniq("itaudd");
    let entity = uuid::Uuid::new_v4().to_string();
    let detail = Log {
        entity_uuid: Some(entity.clone()),
        before: Some(json!({"isActive": true, "deletedAt": null, "someNewField": 1})),
        after: Some(
            json!({"isActive": false, "deletedAt": "2026-01-01T00:00:00.000Z", "someNewField": 2}),
        ),
        changed: vec!["isActive".into(), "deletedAt".into(), "someNewField".into()],
        actor: Some((fx.admin.id, "Budi")),
        ..Log::new(fx.id, "store", &format!("{p} x"), "delete")
    }
    .insert(app.db())
    .await;
    let other_company = Log::new(OTHER_COMPANY, "store", &p, "update")
        .insert(app.db())
        .await;

    let r = app
        .get(&format!("{BASE}/{detail}"))
        .user(fx.admin.id)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(r.json["id"], detail);
    assert_eq!(r.json["before"]["isActive"], true);
    assert_eq!(r.json["after"]["someNewField"], 2);
    assert_eq!(
        r.json["diff"],
        json!([
            {"field": "isActive", "label": "Status", "before": "Active", "after": "Inactive"},
            {"field": "deletedAt", "label": "Status", "before": "Active", "after": "Deleted"},
            {"field": "someNewField", "label": "Some New Field", "before": 1, "after": 2},
        ])
    );

    // 404: tidak ada, atau milik company lain (tidak dibedakan).
    for u in [MISSING.to_owned(), other_company.clone()] {
        let r = app
            .get(&format!("{BASE}/{u}"))
            .user(fx.admin.id)
            .send()
            .await;
        r.assert_status(404);
        assert_eq!(
            r.json,
            json!({"code": "AUDIT_LOG_NOT_FOUND", "message": "Audit log not found"})
        );
    }

    // Riwayat entitas.
    let r = app
        .get(&format!("{BASE}/entities/store/{entity}"))
        .user(fx.manager.id)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(ids(&r), vec![detail.clone()]);
    let r = app
        .get(&format!("{BASE}/entities/geofence/{entity}"))
        .user(fx.manager.id)
        .send()
        .await;
    assert_eq!(total(&r), 0);
    let r = app
        .get(&format!("{BASE}/entities/bogus/{entity}"))
        .user(fx.manager.id)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json,
        json!({"message": "Validation failed (enum string is expected)", "error": "Bad Request", "statusCode": 400})
    );
    app.get(&format!("{BASE}/entities/store/{entity}?pageNumber=0"))
        .user(fx.manager.id)
        .send()
        .await
        .assert_status(400);

    // Peran dua tingkat: MANAGER boleh baca, tidak boleh restore; STAFF tidak boleh apa pun.
    for u in [&fx.manager, &fx.admin, &fx.super_admin] {
        app.get(BASE).user(u.id).send().await.assert_status(200);
        app.get(&format!("{BASE}/recycle-bin"))
            .user(u.id)
            .send()
            .await
            .assert_status(200);
    }
    app.get(BASE)
        .user(fx.staff.id)
        .send()
        .await
        .assert_status(401);
    app.get(BASE).send().await.assert_status(401);
    app.post(&format!("{BASE}/{detail}/restore"))
        .user(fx.manager.id)
        .send()
        .await
        .assert_status(401);
    app.post(&format!("{BASE}/{detail}/restore"))
        .user(fx.staff.id)
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
async fn recycle_bin_only_lists_open_restorable_deletes() {
    let _g = PLAN.read().await;
    let Some((app, fx)) = setup().await else {
        return;
    };
    let p = uniq("itaudb");
    let mk = |et: &'static str, action: &'static str, restorable: bool| Log {
        restorable,
        ..Log::new(fx.id, et, &format!("{p} bin"), action)
    };
    let yes1 = mk("store", "delete", true).insert(app.db()).await;
    let yes2 = Log {
        age_days: 1.0,
        ..mk("geofence", "delete", true)
    }
    .insert(app.db())
    .await;
    mk("store", "delete", false).insert(app.db()).await;
    mk("store", "update", true).insert(app.db()).await;
    let restored = mk("store", "delete", true).insert(app.db()).await;
    sqlx::query(r#"UPDATE "auditLog" SET "restoredAt" = now() WHERE uuid = $1::uuid"#)
        .bind(&restored)
        .execute(app.db())
        .await
        .unwrap();
    Log {
        restorable: true,
        ..Log::new(OTHER_COMPANY, "store", &format!("{p} bin"), "delete")
    }
    .insert(app.db())
    .await;

    let r = app
        .get(&format!("{BASE}/recycle-bin?search={p}"))
        .user(fx.manager.id)
        .send()
        .await;
    r.assert_status(200);
    assert_eq!(ids(&r), vec![yes1.clone(), yes2.clone()]);
    assert_eq!(r.json["data"][0]["isRestorable"], true);
    assert_eq!(r.json["data"][0]["action"], "delete");
    let r = app
        .get(&format!(
            "{BASE}/recycle-bin?search={p}&entityType=geofence"
        ))
        .user(fx.manager.id)
        .send()
        .await;
    assert_eq!(ids(&r), vec![yes2]);
    // Filter khusus daftar tidak berlaku di kotak sampah.
    let r = app
        .get(&format!("{BASE}/recycle-bin?search={p}&action=create"))
        .user(fx.manager.id)
        .send()
        .await;
    assert_eq!(total(&r), 2);
}

#[tokio::test]
async fn plan_without_audit_log_is_forbidden() {
    let _g = PLAN.write().await;
    let Some((app, fx)) = setup().await else {
        return;
    };
    open_plan(&app, fx.id, false).await;
    for (method, uri) in [
        ("GET", BASE.to_owned()),
        ("GET", format!("{BASE}/recycle-bin")),
        ("GET", format!("{BASE}/{MISSING}")),
        ("GET", format!("{BASE}/entities/store/{MISSING}")),
        ("POST", format!("{BASE}/{MISSING}/restore")),
    ] {
        let req = if method == "GET" {
            app.get(&uri)
        } else {
            app.post(&uri)
        };
        let r = req.user(fx.admin.id).send().await;
        r.assert_status(403);
        assert_eq!(
            r.json,
            json!({"code": "PLAN_FEATURE_DISABLED", "message": "Audit Log is not included in this company's plan"}),
            "{uri}"
        );
    }
    // Peran (guard) jalan lebih dulu daripada gerbang paket.
    app.get(BASE)
        .user(fx.staff.id)
        .send()
        .await
        .assert_status(401);
    open_plan(&app, fx.id, true).await;
    app.get(BASE)
        .user(fx.admin.id)
        .send()
        .await
        .assert_status(200);
}

// ── restore ─────────────────────────────────────────────────────────────────

struct Fx<'a> {
    app: &'a TestApp,
    fx: &'a Company,
}

impl Fx<'_> {
    async fn store(
        &self,
        code: &str,
        deleted: bool,
        is_active: bool,
        group: Option<i32>,
    ) -> (i32, String) {
        sqlx::query_as(
            r#"INSERT INTO store ("companyId", code, name, "ownerName", address, lat, lng, "createdById",
                                  "isActive", "deletedAt", "groupId")
               VALUES ($1, $2, $2, 'Owner', 'Jl. Test', -7.8, 110.3, $3, $4,
                       CASE WHEN $5 THEN now() AT TIME ZONE 'utc' END, $6)
               RETURNING id, uuid::text"#,
        )
        .bind(self.fx.id)
        .bind(code)
        .bind(self.fx.admin.id)
        .bind(is_active)
        .bind(deleted)
        .bind(group)
        .fetch_one(self.app.db())
        .await
        .unwrap()
    }

    /// Snapshot `before` lengkap seperti yang ditulis middleware: proyeksi baris aslinya
    /// dengan `deletedAt = null`.
    async fn snapshot(&self, model: mz_audit::Model, id: i32) -> Value {
        let row = mz_audit::read_rows(
            self.app.db(),
            model,
            mz_audit::Columns::All,
            &mz_audit::Lookup::Id(id),
            None,
            Some(1),
        )
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
        let mut snap = mz_audit::project_row(model, Some(&row)).unwrap().to_json();
        snap["deletedAt"] = Value::Null;
        snap
    }

    /// Baris `delete` restorable untuk entitas (snapshot `before`/`after` ala middleware).
    async fn delete_log(
        &self,
        et: &'static str,
        id: i32,
        uuid: &str,
        label: &str,
        before: Value,
    ) -> String {
        self.log_with(et, id, uuid, label, before, true, 0.0).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn log_with(
        &self,
        et: &'static str,
        id: i32,
        uuid: &str,
        label: &str,
        before: Value,
        restorable: bool,
        age_days: f64,
    ) -> String {
        let mut after = before.clone();
        if let Some(o) = after.as_object_mut() {
            o.insert("deletedAt".into(), json!("2026-11-16T00:00:00.000Z"));
        }
        Log {
            entity_id: id,
            entity_uuid: Some(uuid.to_owned()),
            actor: Some((self.fx.admin.id, "Budi Santoso")),
            before: Some(before),
            after: Some(after),
            changed: vec!["deletedAt".into()],
            restorable,
            age_days,
            ..Log::new(self.fx.id, et, label, "delete")
        }
        .insert(self.app.db())
        .await
    }

    async fn restore(&self, log: &str, user: i32) -> TestResponse {
        self.app
            .post(&format!("{BASE}/{log}/restore"))
            .user(user)
            .send()
            .await
    }

    async fn restore_rows(&self, entity: &str, id: i32) -> i64 {
        sqlx::query_scalar(
            r#"SELECT count(*) FROM "auditLog"
                WHERE "companyId" = $1 AND "entityType"::text = $2 AND "entityId" = $3 AND action = 'restore'"#,
        )
        .bind(self.fx.id)
        .bind(entity)
        .bind(id)
        .fetch_one(self.app.db())
        .await
        .unwrap()
    }

    async fn marked(&self, log: &str) -> bool {
        sqlx::query_scalar(
            r#"SELECT "restoredAt" IS NOT NULL FROM "auditLog" WHERE uuid = $1::uuid"#,
        )
        .bind(log)
        .fetch_one(self.app.db())
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn restore_store_writes_restore_row_marks_deletes_and_is_single_use() {
    let _g = PLAN.read().await;
    let Some((app, fx)) = setup().await else {
        return;
    };
    let t = Fx { app: &app, fx: &fx };
    let code = uniq("ITR").to_uppercase();
    let (sid, suuid) = t.store(&code, true, true, None).await;
    let snapshot = t.snapshot(mz_audit::Model::Store, sid).await;
    let del = t.delete_log("store", sid, &suuid, &code, snapshot).await;
    // Baris `delete` kembar (transaksi di-retry) ikut ditandai.
    let twin = t
        .delete_log(
            "store",
            sid,
            &suuid,
            &code,
            json!({"isActive": true, "deletedAt": null}),
        )
        .await;

    let r = t.restore(&del, fx.admin.id).await;
    r.assert_status(200);
    let body = &r.json;
    assert_eq!(body["id"], del);
    assert_eq!(body["entityType"], "store");
    assert_eq!(body["entity"], json!({"id": suuid, "label": code}));
    assert_eq!(body["warnings"], json!([]));
    assert_eq!(body["notRestored"], json!([]));
    assert!(body["restoredAt"].is_i64());
    assert_eq!(body["after"]["deletedAt"], Value::Null);
    assert_eq!(body["after"]["isActive"], true);
    assert_eq!(body["after"]["code"], code);
    assert!(body["after"].get("id").is_none());
    let restore_uuid = body["restoreLogId"].as_str().unwrap().to_owned();

    // Entitas pulih.
    let deleted_at: Option<chrono::NaiveDateTime> =
        sqlx::query_scalar("SELECT \"deletedAt\" FROM store WHERE id = $1")
            .bind(sid)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert!(deleted_at.is_none());

    // Tepat satu baris `restore`, dari pelaku yang benar, dengan snapshot terhapus → pulih.
    #[allow(clippy::type_complexity)]
    let row: (
        String,
        i32,
        Option<i32>,
        bool,
        Value,
        Value,
        Vec<String>,
        Option<String>,
    ) = sqlx::query_as(
        r#"SELECT action::text, "companyId", "actorId", "isRestorable", "before", "after", "changedFields",
                  "entityLabel"
             FROM "auditLog" WHERE uuid = $1::uuid"#,
    )
    .bind(&restore_uuid)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(row.0, "restore");
    assert_eq!(row.1, fx.id);
    assert_eq!(row.2, Some(fx.admin.id));
    assert!(!row.3);
    assert_eq!(row.4["deletedAt"], "2026-11-16T00:00:00.000Z");
    assert_eq!(row.5["deletedAt"], Value::Null);
    assert_eq!(row.6, vec!["deletedAt".to_owned()]);
    assert_eq!(row.7.as_deref(), Some(code.as_str()));
    assert_eq!(t.restore_rows("store", sid).await, 1);

    // Seluruh baris `delete` entitas ditandai menunjuk baris `restore`.
    for log in [&del, &twin] {
        let m: (bool, Option<i32>, Option<String>) = sqlx::query_as(
            r#"SELECT "restoredAt" IS NOT NULL, "restoredById", "restoredByLogId"::text
                 FROM "auditLog" WHERE uuid = $1::uuid"#,
        )
        .bind(log)
        .fetch_one(app.db())
        .await
        .unwrap();
        assert_eq!(m, (true, Some(fx.admin.id), Some(restore_uuid.clone())));
    }

    // Sekali pakai: kedua kalinya 409 dengan rincian.
    let r2 = t.restore(&del, fx.admin.id).await;
    r2.assert_status(409);
    assert_eq!(r2.json["code"], "ALREADY_RESTORED");
    assert_eq!(
        r2.json["message"],
        "This deletion has already been restored"
    );
    assert_eq!(r2.json["details"]["restoredByLogId"], restore_uuid);
    assert!(r2.json["details"]["restoredAt"].is_i64());
    t.restore(&twin, fx.super_admin.id).await.assert_status(409);
    assert_eq!(t.restore_rows("store", sid).await, 1);
    // Sudah tidak ada di kotak sampah.
    let bin = app
        .get(&format!("{BASE}/recycle-bin?search={code}"))
        .user(fx.admin.id)
        .send()
        .await;
    assert_eq!(total(&bin), 0);
}

#[tokio::test]
async fn restore_store_takes_isactive_from_snapshot_and_reports_archived_group_and_customer() {
    let _g = PLAN.read().await;
    let Some((app, fx)) = setup().await else {
        return;
    };
    let t = Fx { app: &app, fx: &fx };
    let gcode = uniq("ITG").to_uppercase();
    let group: i32 = sqlx::query_scalar(
        r#"INSERT INTO "storeGroup" ("companyId", code, name, "createdById", "isActive")
           VALUES ($1, $2, $2, $3, false) RETURNING id"#,
    )
    .bind(fx.id)
    .bind(&gcode)
    .bind(fx.admin.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    let customer: i32 = sqlx::query_scalar(
        r#"INSERT INTO customer ("companyId", code, name, "createdById", "isActive", "deletedAt")
           VALUES ($1, $2, $2, $3, true, now() AT TIME ZONE 'utc') RETURNING id"#,
    )
    .bind(fx.id)
    .bind(uniq("ITK").to_uppercase())
    .bind(fx.admin.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    let code = uniq("ITS").to_uppercase();
    let (sid, suuid) = t.store(&code, true, false, Some(group)).await;
    sqlx::query(r#"UPDATE store SET "customerId" = $2 WHERE id = $1"#)
        .bind(sid)
        .bind(customer)
        .execute(app.db())
        .await
        .unwrap();
    let del = t
        .delete_log(
            "store",
            sid,
            &suuid,
            &code,
            json!({"isActive": false, "deletedAt": null}),
        )
        .await;
    let r = t.restore(&del, fx.admin.id).await;
    r.assert_status(200);
    // `isActive` dari snapshot (false), bukan dipaksa true.
    assert_eq!(r.json["after"]["isActive"], false);
    assert_eq!(
        r.json["notRestored"],
        json!([
            {"what": "storeGroup", "reason": "This store's region has been archived",
             "endpoint": "PATCH /v1/admin/stores/:uuid"},
            {"what": "customer", "reason": "This store's customer has been deleted",
             "endpoint": "PATCH /v1/admin/stores/:uuid"},
        ])
    );
    let active: bool = sqlx::query_scalar(r#"SELECT "isActive" FROM store WHERE id = $1"#)
        .bind(sid)
        .fetch_one(app.db())
        .await
        .unwrap();
    assert!(!active);
}

#[tokio::test]
async fn restore_rejections_leave_everything_untouched() {
    let _g = PLAN.read().await;
    let Some((app, fx)) = setup().await else {
        return;
    };
    let t = Fx { app: &app, fx: &fx };

    // Kode toko sudah dipakai toko aktif lain → 409 STORE_CODE_TAKEN, tidak ada yang berubah.
    let code = uniq("ITC").to_uppercase();
    let (deleted_id, deleted_uuid) = t.store(&code, true, true, None).await;
    t.store(&code, false, true, None).await;
    let del = t
        .delete_log(
            "store",
            deleted_id,
            &deleted_uuid,
            &code,
            json!({"isActive": true, "deletedAt": null}),
        )
        .await;
    let r = t.restore(&del, fx.admin.id).await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "STORE_CODE_TAKEN");
    assert_eq!(
        r.json["message"],
        format!("Store code {code} is already used by an active store in this company")
    );
    assert_eq!(r.json["details"], json!({"code": code}));
    let still_deleted: bool =
        sqlx::query_scalar(r#"SELECT "deletedAt" IS NOT NULL FROM store WHERE id = $1"#)
            .bind(deleted_id)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert!(still_deleted);
    assert_eq!(t.restore_rows("store", deleted_id).await, 0);
    assert!(!t.marked(&del).await);

    // MANAGER tidak boleh restore (dan tidak ada yang berubah).
    t.restore(&del, fx.manager.id).await.assert_status(401);
    assert!(!t.marked(&del).await);

    // Bukan penghapusan yang bisa dipulihkan.
    let nr = t
        .log_with(
            "store",
            deleted_id,
            &deleted_uuid,
            &code,
            json!({}),
            false,
            0.0,
        )
        .await;
    let r = t.restore(&nr, fx.admin.id).await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "NOT_RESTORABLE");
    assert_eq!(
        r.json["message"],
        "This audit entry cannot be restored: it is not a user deletion with a complete snapshot"
    );
    assert_eq!(
        r.json["details"],
        json!({"action": "delete", "subEntity": null})
    );

    // Jendela 90 hari.
    let old = t
        .log_with(
            "store",
            deleted_id,
            &deleted_uuid,
            &code,
            json!({"isActive": true}),
            true,
            91.0,
        )
        .await;
    let r = t.restore(&old, fx.admin.id).await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "RESTORE_WINDOW_EXPIRED");
    assert_eq!(r.json["message"], "Restore window of 90 days has passed");
    let created = r.json["details"]["createdAt"].as_i64().unwrap();
    assert_eq!(
        r.json["details"]["expiresAt"].as_i64().unwrap(),
        created + 90 * 86_400_000
    );
    // Baru 89 hari: masih lolos pemeriksaan jendela (jatuh ke bentrok kode yang sama seperti di atas).
    let young = t
        .log_with(
            "store",
            deleted_id,
            &deleted_uuid,
            &code,
            json!({"isActive": true}),
            true,
            89.0,
        )
        .await;
    assert_eq!(
        t.restore(&young, fx.admin.id).await.json["code"],
        "STORE_CODE_TAKEN"
    );

    // Snapshot tak lengkap.
    let inc = t
        .delete_log(
            "store",
            deleted_id,
            &deleted_uuid,
            &code,
            json!({"code": code}),
        )
        .await;
    let r = t.restore(&inc, fx.admin.id).await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "NOT_RESTORABLE");
    assert_eq!(
        r.json["message"],
        "Snapshot is missing \"isActive\", so this entry cannot be restored"
    );
    assert_eq!(r.json["details"], json!({"field": "isActive"}));

    // Baris yang dituju sudah tidak ada → 409 RESTORE_TARGET_MISSING, penandaan dibatalkan.
    let gone = t
        .delete_log(
            "store",
            2_000_000_000,
            MISSING,
            &code,
            json!({"isActive": true}),
        )
        .await;
    let r = t.restore(&gone, fx.admin.id).await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "RESTORE_TARGET_MISSING");
    assert_eq!(
        r.json["message"],
        "The store this entry points at no longer exists"
    );
    assert!(!t.marked(&gone).await);
    assert_eq!(t.restore_rows("store", 2_000_000_000).await, 0);

    // Entitas tanpa jalur restore (role) yang entah bagaimana ber-isRestorable.
    let role = t
        .delete_log("role", 1, MISSING, &code, json!({"status": 1}))
        .await;
    let r = t.restore(&role, fx.admin.id).await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "RESTORE_TARGET_MISSING");
    assert_eq!(
        r.json["message"],
        "The role this entry points at no longer exists"
    );

    // 404: uuid tak dikenal / milik company lain.
    let other = Log {
        restorable: true,
        ..Log::new(OTHER_COMPANY, "store", &code, "delete")
    }
    .insert(app.db())
    .await;
    for u in [MISSING.to_owned(), other.clone()] {
        let r = t.restore(&u, fx.admin.id).await;
        r.assert_status(404);
        assert_eq!(
            r.json,
            json!({"code": "AUDIT_LOG_NOT_FOUND", "message": "Audit log not found"})
        );
    }
    assert!(!t.marked(&other).await);
}

#[tokio::test]
async fn concurrent_restores_produce_exactly_one_restore_row() {
    let _g = PLAN.read().await;
    let Some((app, fx)) = setup().await else {
        return;
    };
    let t = Fx { app: &app, fx: &fx };
    let (gid, guuid): (i32, String) = sqlx::query_as(
        r#"INSERT INTO geofence ("companyId", name, lat, lng, "radiusMeters", "createdById", "isActive")
           VALUES ($1, $2, -7.8, 110.3, 100, $3, false) RETURNING id, uuid::text"#,
    )
    .bind(fx.id)
    .bind(uniq("itgeo"))
    .bind(fx.admin.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    let del = t
        .log_with(
            "geofence",
            gid,
            &guuid,
            "geo",
            json!({"isActive": true}),
            true,
            0.0,
        )
        .await;
    let (r1, r2) = tokio::join!(
        t.restore(&del, fx.admin.id),
        t.restore(&del, fx.super_admin.id)
    );
    let mut statuses = [r1.status.as_u16(), r2.status.as_u16()];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 409], "{:?} / {:?}", r1.json, r2.json);
    let loser = if r1.status.as_u16() == 409 { &r1 } else { &r2 };
    assert_eq!(loser.json["code"], "ALREADY_RESTORED");
    // Pemenang menulis satu baris; baris milik yang kalah ikut dibatalkan bersama transaksinya.
    assert_eq!(t.restore_rows("geofence", gid).await, 1);
    let active: bool = sqlx::query_scalar(r#"SELECT "isActive" FROM geofence WHERE id = $1"#)
        .bind(gid)
        .fetch_one(app.db())
        .await
        .unwrap();
    assert!(active);
}

#[tokio::test]
async fn restore_store_group_form_customer_and_plan() {
    let _g = PLAN.read().await;
    let Some((app, fx)) = setup().await else {
        return;
    };
    let t = Fx { app: &app, fx: &fx };

    // storeGroup: didelegasikan; uuid tak dikenal → 404 STORE_GROUP_NOT_FOUND.
    let (gid, guuid): (i32, String) = sqlx::query_as(
        r#"INSERT INTO "storeGroup" ("companyId", code, name, "createdById", "isActive")
           VALUES ($1, $2, $2, $3, false) RETURNING id, uuid::text"#,
    )
    .bind(fx.id)
    .bind(uniq("ITG").to_uppercase())
    .bind(fx.admin.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    let del = Log {
        entity_id: gid,
        entity_uuid: Some(guuid.clone()),
        actor: Some((fx.admin.id, "Budi")),
        before: Some(json!({"isActive": true})),
        after: Some(json!({"isActive": false})),
        changed: vec!["isActive".into()],
        restorable: true,
        ..Log::new(fx.id, "storeGroup", "grp", "delete")
    }
    .insert(app.db())
    .await;
    let r = t.restore(&del, fx.admin.id).await;
    r.assert_status(200);
    assert_eq!(r.json["after"]["isActive"], true);
    assert_eq!(r.json["entityType"], "storeGroup");
    assert_eq!(t.restore_rows("storeGroup", gid).await, 1);
    let active: bool = sqlx::query_scalar(r#"SELECT "isActive" FROM "storeGroup" WHERE id = $1"#)
        .bind(gid)
        .fetch_one(app.db())
        .await
        .unwrap();
    assert!(active);
    let nogroup = Log {
        entity_id: 2_000_000_000,
        restorable: true,
        before: Some(json!({"isActive": true})),
        after: Some(json!({"isActive": false})),
        ..Log::new(fx.id, "storeGroup", "grp", "delete")
    }
    .insert(app.db())
    .await;
    let r = t.restore(&nogroup, fx.admin.id).await;
    r.assert_status(404);
    assert_eq!(
        r.json,
        json!({"code": "STORE_GROUP_NOT_FOUND", "message": "Store group not found"})
    );

    // storeVisitForm: isActive dari snapshot.
    let (fid, fuuid): (i32, String) = sqlx::query_as(
        r#"INSERT INTO "storeVisitFormTemplate" ("companyId", name, "createdById", "isActive", "deletedAt")
           VALUES ($1, $2, $3, false, now() AT TIME ZONE 'utc') RETURNING id, uuid::text"#,
    )
    .bind(fx.id)
    .bind(uniq("itform"))
    .bind(fx.admin.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    let del = t
        .delete_log(
            "storeVisitForm",
            fid,
            &fuuid,
            "form",
            json!({"isActive": false, "deletedAt": null}),
        )
        .await;
    let r = t.restore(&del, fx.admin.id).await;
    r.assert_status(200);
    assert_eq!(r.json["after"]["isActive"], false);
    assert_eq!(r.json["after"]["deletedAt"], Value::Null);

    // customer: bentrok kode → 409; lalu setelah kode bebas → sukses dengan isActive dari snapshot.
    let ccode = uniq("ITK").to_uppercase();
    let (cid, cuuid): (i32, String) = sqlx::query_as(
        r#"INSERT INTO customer ("companyId", code, name, "createdById", "isActive", "deletedAt")
           VALUES ($1, $2, $2, $3, true, now() AT TIME ZONE 'utc') RETURNING id, uuid::text"#,
    )
    .bind(fx.id)
    .bind(&ccode)
    .bind(fx.admin.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    let clash: i32 = sqlx::query_scalar(
        r#"INSERT INTO customer ("companyId", code, name, "createdById") VALUES ($1, $2, $2, $3) RETURNING id"#,
    )
    .bind(fx.id)
    .bind(&ccode)
    .bind(fx.admin.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    let del = t
        .delete_log(
            "customer",
            cid,
            &cuuid,
            &ccode,
            json!({"isActive": true, "deletedAt": null}),
        )
        .await;
    let r = t.restore(&del, fx.admin.id).await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "CUSTOMER_CODE_TAKEN");
    assert_eq!(
        r.json["message"],
        format!("Customer code {ccode} is already used in this company")
    );
    sqlx::query(r#"UPDATE customer SET "deletedAt" = now() AT TIME ZONE 'utc' WHERE id = $1"#)
        .bind(clash)
        .execute(app.db())
        .await
        .unwrap();
    let r = t.restore(&del, fx.admin.id).await;
    r.assert_status(200);
    assert_eq!(r.json["after"]["isActive"], true);
    assert!(r.json["after"].get("taxNumber").is_none());

    // storeVisitPlan: toko terhapus 422, sales terhapus 422, bentrok slot 409, tanggal lampau = warning.
    let pcode = uniq("ITP").to_uppercase();
    let (store_id, _) = t.store(&pcode, false, true, None).await;
    let plan = |store: i32, date: &'static str| {
        let app = &app;
        let (company, user, creator) = (fx.id, fx.staff.id, fx.admin.id);
        async move {
            sqlx::query_as::<_, (i32, String)>(
                r#"INSERT INTO "storeVisitPlan" ("companyId", "userId", "storeId", "visitDate", "createdById", "deletedAt")
                   VALUES ($1, $2, $3, $4::date, $5, now() AT TIME ZONE 'utc') RETURNING id, uuid::text"#,
            )
            .bind(company)
            .bind(user)
            .bind(store)
            .bind(date)
            .bind(creator)
            .fetch_one(app.db())
            .await
            .unwrap()
        }
    };
    let (pid, puuid) = plan(store_id, "2020-01-01").await;
    let del = t
        .delete_log(
            "storeVisitPlan",
            pid,
            &puuid,
            "plan",
            json!({"deletedAt": null}),
        )
        .await;
    let r = t.restore(&del, fx.admin.id).await;
    r.assert_status(200);
    assert_eq!(
        r.json["warnings"],
        json!(["The 2020-01-01 schedule is in the past; the visit can no longer be carried out"])
    );
    // Slot yang sama sudah terisi (plan di atas aktif) → plan kedua yang terhapus bentrok.
    let (pid2, puuid2) = plan(store_id, "2020-01-01").await;
    let del2 = t
        .delete_log(
            "storeVisitPlan",
            pid2,
            &puuid2,
            "plan",
            json!({"deletedAt": null}),
        )
        .await;
    let r = t.restore(&del2, fx.admin.id).await;
    r.assert_status(409);
    assert_eq!(
        r.json,
        json!({"code": "PLAN_ALREADY_EXISTS",
               "message": "Another plan already occupies this sales, date, and store slot"})
    );
    // Toko terhapus → 422.
    let (dstore, _) = t.store(&uniq("ITD").to_uppercase(), true, true, None).await;
    let (pid3, puuid3) = plan(dstore, "2020-01-02").await;
    let del3 = t
        .delete_log(
            "storeVisitPlan",
            pid3,
            &puuid3,
            "plan",
            json!({"deletedAt": null}),
        )
        .await;
    let r = t.restore(&del3, fx.admin.id).await;
    r.assert_status(422);
    assert_eq!(
        r.json,
        json!({"code": "STORE_UNAVAILABLE", "message": "The store of this plan has been deleted"})
    );
    assert!(!t.marked(&del3).await);
}

#[tokio::test]
async fn restore_employee_seat_limit_shared_account_and_token_revocation() {
    let _g = PLAN.read().await;
    let Some((app, fx)) = setup().await else {
        return;
    };
    let t = Fx { app: &app, fx: &fx };
    let new_user = |status: i32| {
        let app = &app;
        async move {
            sqlx::query_as::<_, (i32, String)>(
                r#"INSERT INTO users (username, password, name, status)
                   VALUES ($1, 'x', 'Karyawan Uji', $2) RETURNING id, uuid::text"#,
            )
            .bind(format!("{}@it.test", uniq("itemp")))
            .bind(status)
            .fetch_one(app.db())
            .await
            .unwrap()
        }
    };

    // Kuota company fixture menentukan jalur yang bisa diuji.
    let max_users: i32 = sqlx::query_scalar(r#"SELECT "maxUsers" FROM company WHERE id = $1"#)
        .bind(fx.id)
        .fetch_one(app.db())
        .await
        .unwrap();
    let used: i64 = sqlx::query_scalar(
        r#"SELECT count(*) FROM users WHERE status <> 0 AND id IN
             (SELECT "userId" FROM "userCompany" WHERE "companyId" = $1 AND status = 1)"#,
    )
    .bind(fx.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    let (uid, uuuid) = new_user(0).await;
    let del = t
        .delete_log(
            "employee",
            uid,
            &uuuid,
            "Karyawan Uji",
            json!({"status": 1}),
        )
        .await;
    if max_users > 0 && used >= i64::from(max_users) {
        let r = t.restore(&del, fx.admin.id).await;
        r.assert_status(409);
        assert_eq!(r.json["code"], "SEAT_LIMIT_REACHED");
        assert_eq!(
            r.json["message"],
            "This company has no seat left for the restored employee"
        );
        assert_eq!(
            r.json["details"],
            json!({"maxUsers": max_users, "used": used})
        );
        return;
    }

    // Akun bersama: aktif di company lain → status tak boleh diubah (403) dan tak ada yang berubah.
    let (shared_id, shared_uuid) = new_user(0).await;
    let other_company: i32 =
        sqlx::query_scalar(r#"INSERT INTO company (name, status) VALUES ($1, 1) RETURNING id"#)
            .bind(uniq("itcomp"))
            .fetch_one(app.db())
            .await
            .unwrap();
    sqlx::query(
        r#"INSERT INTO "userCompany" ("userId", "companyId", status, "idleTimeInSecond", "isDefault")
           VALUES ($1, $2, 1, 300, true)"#,
    )
    .bind(shared_id)
    .bind(other_company)
    .execute(app.db())
    .await
    .unwrap();
    let shared_del = t
        .delete_log(
            "employee",
            shared_id,
            &shared_uuid,
            "Akun Bersama",
            json!({"status": 1}),
        )
        .await;
    let r = t.restore(&shared_del, fx.admin.id).await;
    r.assert_status(403);
    assert_eq!(
        r.json,
        json!({"code": "TARGET_SHARED_ACCOUNT",
               "message": "This account also belongs to another company; its login, name and status can only be changed by the account owner."})
    );
    let status: i32 = sqlx::query_scalar("SELECT status FROM users WHERE id = $1")
        .bind(shared_id)
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(status, 0);
    assert_eq!(t.restore_rows("employee", shared_id).await, 0);
    assert!(!t.marked(&shared_del).await);

    // Sukses: status dari snapshot; keanggotaan & peran dilaporkan belum pulih.
    let r = t.restore(&del, fx.admin.id).await;
    r.assert_status(200);
    assert_eq!(r.json["after"]["status"], 1);
    assert!(r.json["after"].get("password").is_none());
    assert_eq!(
        r.json["notRestored"],
        json!([
            {"what": "companyMembership", "reason": "This employee's company membership is not active",
             "endpoint": "PATCH /users/:uuid"},
            {"what": "accessLevel", "reason": "This employee has no active role in the company",
             "endpoint": "PATCH /users/:uuid"},
        ])
    );
    let status: i32 = sqlx::query_scalar("SELECT status FROM users WHERE id = $1")
        .bind(uid)
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(status, 1);
    let revoked: bool =
        sqlx::query_scalar(r#"SELECT "tokensValidAfter" IS NOT NULL FROM users WHERE id = $1"#)
            .bind(uid)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert!(!revoked, "snapshot ACTIVE tidak mencabut token");

    // Snapshot DISABLED: token lama dicabut di transaksi yang sama.
    let (did, duuid) = new_user(0).await;
    let ddel = t
        .delete_log(
            "employee",
            did,
            &duuid,
            "Karyawan Nonaktif",
            json!({"status": 3}),
        )
        .await;
    t.restore(&ddel, fx.admin.id).await.assert_status(200);
    let (status, revoked): (i32, bool) =
        sqlx::query_as(r#"SELECT status, "tokensValidAfter" IS NOT NULL FROM users WHERE id = $1"#)
            .bind(did)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(status, 3);
    assert!(revoked);
}

#[tokio::test]
async fn restore_product_sku_clash_and_deleted_price_items() {
    let _g = PLAN.read().await;
    let Some((app, fx)) = setup().await else {
        return;
    };
    let t = Fx { app: &app, fx: &fx };
    let db = app.db();
    let master = |table: &'static str| {
        let code = uniq("ITM").to_uppercase();
        let (company, creator) = (fx.id, fx.admin.id);
        async move {
            sqlx::query_scalar::<_, i32>(&format!(
                r#"INSERT INTO "{table}" ("companyId", code, name, "createdById")
                   VALUES ($1, $2, $2, $3) RETURNING id"#
            ))
            .bind(company)
            .bind(code)
            .bind(creator)
            .fetch_one(db)
            .await
            .unwrap()
        }
    };
    let category = master("productCategory").await;
    let uom = master("uom").await;
    let product = |sku: String, deleted: bool, is_active: bool| {
        let (company, creator) = (fx.id, fx.admin.id);
        async move {
            sqlx::query_as::<_, (i32, String)>(
                r#"INSERT INTO product ("companyId", sku, name, "categoryId", "baseUomId", "createdById",
                                        "isActive", "deletedAt")
                   VALUES ($1, $2, $2, $3, $4, $5, $6, CASE WHEN $7 THEN now() AT TIME ZONE 'utc' END)
                   RETURNING id, uuid::text"#,
            )
            .bind(company)
            .bind(sku)
            .bind(category)
            .bind(uom)
            .bind(creator)
            .bind(is_active)
            .bind(deleted)
            .fetch_one(db)
            .await
            .unwrap()
        }
    };
    let price_list: i32 = sqlx::query_scalar(
        r#"INSERT INTO "priceList" ("companyId", code, name, "currencyCode", "createdById")
           VALUES ($1, $2, $2, 'IDR', $3) RETURNING id"#,
    )
    .bind(fx.id)
    .bind(uniq("ITPL").to_uppercase())
    .bind(fx.admin.id)
    .fetch_one(db)
    .await
    .unwrap();
    let price_item = |product: i32, deleted: bool| {
        let company = fx.id;
        async move {
            sqlx::query(
                r#"INSERT INTO "priceListItem" ("companyId", "priceListId", "productId", "uomId", price, "deletedAt")
                   VALUES ($1, $2, $3, $4, 1000, CASE WHEN $5 THEN now() AT TIME ZONE 'utc' END)"#,
            )
            .bind(company)
            .bind(price_list)
            .bind(product)
            .bind(uom)
            .bind(deleted)
            .execute(db)
            .await
            .unwrap();
        }
    };
    let deleted_at = |id: i32| async move {
        sqlx::query_scalar::<_, Option<chrono::NaiveDateTime>>(
            r#"SELECT "deletedAt" FROM product WHERE id = $1"#,
        )
        .bind(id)
        .fetch_one(db)
        .await
        .unwrap()
    };

    // Produk terhapus (di DB `isActive = false`; snapshot `true`) dengan satu harga ikut terhapus.
    let sku = uniq("ITSKU").to_uppercase();
    let (pid, puuid) = product(sku.clone(), true, false).await;
    price_item(pid, true).await;
    price_item(pid, false).await;
    // `product` tidak punya `AUDIT_FIELD_POLICIES` di Node → proyeksi kosong; snapshot ditulis tangan.
    let snapshot = t.snapshot(mz_audit::Model::Product, pid).await;
    assert_eq!(snapshot, json!({"deletedAt": null}));
    let del = t
        .delete_log(
            "product",
            pid,
            &puuid,
            &sku,
            json!({"isActive": true, "deletedAt": null}),
        )
        .await;

    // SKU dipakai produk aktif lain → 409, tidak ada yang berubah.
    let (clash, _) = product(sku.clone(), false, true).await;
    let r = t.restore(&del, fx.admin.id).await;
    r.assert_status(409);
    assert_eq!(
        r.json,
        json!({"code": "PRODUCT_SKU_TAKEN",
               "message": format!("SKU {sku} is already used by an active product in this company")})
    );
    assert!(deleted_at(pid).await.is_some());
    assert!(!t.marked(&del).await);
    assert_eq!(t.restore_rows("product", pid).await, 0);

    // SKU bebas → pulih, `isActive` dari snapshot, harga yang terhapus dilaporkan.
    sqlx::query(r#"UPDATE product SET "deletedAt" = now() AT TIME ZONE 'utc' WHERE id = $1"#)
        .bind(clash)
        .execute(db)
        .await
        .unwrap();
    let r = t.restore(&del, fx.admin.id).await;
    r.assert_status(200);
    assert_eq!(r.json["entityType"], "product");
    assert_eq!(r.json["entity"], json!({"id": puuid, "label": sku}));
    // `projectRow('product', row)` = `{}` (tanpa kebijakan field), sama seperti Node.
    assert_eq!(r.json["after"], json!({}));
    assert_eq!(r.json["warnings"], json!([]));
    assert_eq!(
        r.json["notRestored"],
        json!([{"what": "priceListItem",
                "reason": "This product's prices were removed when it was deleted and are not restored automatically",
                "endpoint": "PUT /v1/admin/price-lists/:id/items"}])
    );
    assert!(deleted_at(pid).await.is_none());
    let is_active: bool = sqlx::query_scalar(r#"SELECT "isActive" FROM product WHERE id = $1"#)
        .bind(pid)
        .fetch_one(db)
        .await
        .unwrap();
    assert!(is_active);
    // Harga TIDAK ikut dipulihkan.
    let still: i64 = sqlx::query_scalar(
        r#"SELECT count(*) FROM "priceListItem" WHERE "productId" = $1 AND "deletedAt" IS NOT NULL"#,
    )
    .bind(pid)
    .fetch_one(db)
    .await
    .unwrap();
    assert_eq!(still, 1);
    assert_eq!(t.restore_rows("product", pid).await, 1);
    assert!(t.marked(&del).await);

    // Tanpa harga terhapus (hanya harga aktif) → `notRestored` kosong.
    let sku2 = uniq("ITSKU").to_uppercase();
    let (pid2, puuid2) = product(sku2.clone(), true, true).await;
    price_item(pid2, false).await;
    let del2 = t
        .delete_log(
            "product",
            pid2,
            &puuid2,
            &sku2,
            json!({"isActive": true, "deletedAt": null}),
        )
        .await;
    let r = t.restore(&del2, fx.admin.id).await;
    r.assert_status(200);
    assert_eq!(r.json["notRestored"], json!([]));

    // Snapshot tanpa `isActive` → 409 NOT_RESTORABLE.
    let sku3 = uniq("ITSKU").to_uppercase();
    let (pid3, puuid3) = product(sku3.clone(), true, true).await;
    let del3 = t
        .delete_log("product", pid3, &puuid3, &sku3, json!({"deletedAt": null}))
        .await;
    let r = t.restore(&del3, fx.admin.id).await;
    r.assert_status(409);
    assert_eq!(
        r.json,
        json!({"code": "NOT_RESTORABLE",
               "message": "Snapshot is missing \"isActive\", so this entry cannot be restored",
               "details": {"field": "isActive"}})
    );
    assert!(deleted_at(pid3).await.is_some());

    // Produk milik company lain tidak terlihat → 409 RESTORE_TARGET_MISSING.
    let del4 = t
        .delete_log(
            "product",
            2_000_000_000,
            MISSING,
            "ghost",
            json!({"isActive": true, "deletedAt": null}),
        )
        .await;
    let r = t.restore(&del4, fx.admin.id).await;
    r.assert_status(409);
    assert_eq!(
        r.json,
        json!({"code": "RESTORE_TARGET_MISSING",
               "message": "The product this entry points at no longer exists"})
    );
}

#[tokio::test]
async fn restore_company_status_from_snapshot_only_for_own_company() {
    let _g = PLAN.read().await;
    let Some((app, fx)) = setup().await else {
        return;
    };
    let t = Fx { app: &app, fx: &fx };
    let db = app.db();
    let status_of = |id: i32| async move {
        sqlx::query_scalar::<_, i32>("SELECT status FROM company WHERE id = $1")
            .bind(id)
            .fetch_one(db)
            .await
            .unwrap()
    };
    let original = status_of(fx.id).await;
    let company_uuid: String = sqlx::query_scalar("SELECT uuid::text FROM company WHERE id = $1")
        .bind(fx.id)
        .fetch_one(db)
        .await
        .unwrap();

    // Company lain (bukan tenant pemanggil) → 409 RESTORE_TARGET_MISSING, tak ada yang berubah.
    let other: i32 =
        sqlx::query_scalar(r#"INSERT INTO company (name, status) VALUES ($1, 0) RETURNING id"#)
            .bind(uniq("itcomp"))
            .fetch_one(db)
            .await
            .unwrap();
    let foreign = Log {
        entity_id: other,
        actor: Some((fx.admin.id, "Budi")),
        before: Some(json!({"status": 1})),
        after: Some(json!({"status": 0})),
        changed: vec!["status".into()],
        restorable: true,
        ..Log::new(fx.id, "company", "Lain", "delete")
    }
    .insert(db)
    .await;
    let r = t.restore(&foreign, fx.admin.id).await;
    r.assert_status(409);
    assert_eq!(
        r.json,
        json!({"code": "RESTORE_TARGET_MISSING",
               "message": "The company this entry points at no longer exists"})
    );
    assert_eq!(status_of(other).await, 0);
    assert!(!t.marked(&foreign).await);

    // Snapshot tanpa `status` (angka) → 409 NOT_RESTORABLE.
    let broken = Log {
        entity_id: fx.id,
        entity_uuid: Some(company_uuid.clone()),
        actor: Some((fx.admin.id, "Budi")),
        before: Some(json!({"status": "1"})),
        after: Some(json!({"status": 0})),
        changed: vec!["status".into()],
        restorable: true,
        ..Log::new(fx.id, "company", "Sendiri", "delete")
    }
    .insert(db)
    .await;
    let r = t.restore(&broken, fx.admin.id).await;
    r.assert_status(409);
    assert_eq!(
        r.json,
        json!({"code": "NOT_RESTORABLE",
               "message": "Snapshot is missing \"status\", so this entry cannot be restored",
               "details": {"field": "status"}})
    );

    // Tenant sendiri: `status ← before.status`. Penanda `delete` lama (snapshot rusak di atas)
    // ikut ditandai karena entitasnya sama.
    let target = original + 7;
    let own = Log {
        entity_id: fx.id,
        entity_uuid: Some(company_uuid.clone()),
        actor: Some((fx.admin.id, "Budi")),
        before: Some(json!({"status": target})),
        after: Some(json!({"status": original})),
        changed: vec!["status".into()],
        restorable: true,
        ..Log::new(fx.id, "company", "Sendiri", "delete")
    }
    .insert(db)
    .await;
    let r = t.restore(&own, fx.admin.id).await;
    // Kembalikan status fixture apa pun hasilnya.
    let restored = status_of(fx.id).await;
    sqlx::query("UPDATE company SET status = $2 WHERE id = $1")
        .bind(fx.id)
        .bind(original)
        .execute(db)
        .await
        .unwrap();
    r.assert_status(200);
    assert_eq!(restored, target);
    assert_eq!(r.json["entityType"], "company");
    assert_eq!(
        r.json["entity"],
        json!({"id": company_uuid, "label": "Sendiri"})
    );
    assert_eq!(r.json["after"]["status"], target);
    assert_eq!(r.json["notRestored"], json!([]));
    assert_eq!(t.restore_rows("company", fx.id).await, 1);
    assert!(t.marked(&own).await);
    assert!(t.marked(&broken).await);
}

// ── prune ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn prune_removes_expired_rows_in_batches_and_tombstones_only() {
    // Eksklusif: prune menghapus SEMUA baris audit > 90 hari, termasuk seed test lain.
    let _g = PLAN.write().await;
    let Some((app, fx)) = setup().await else {
        return;
    };
    let t = Fx { app: &app, fx: &fx };
    let started = Utc::now().naive_utc() - Duration::seconds(1);
    let p = uniq("itprune");
    let mk = |age: f64| Log {
        age_days: age,
        ..Log::new(fx.id, "store", &p, "update")
    };
    // 5 kedaluwarsa (batch 2 → 3 putaran), 2 masih dalam retensi.
    for age in [91.0, 100.0, 120.0, 200.0, 365.0] {
        mk(age).insert(app.db()).await;
    }
    let keep1 = mk(89.0).insert(app.db()).await;
    let keep2 = mk(1.0).insert(app.db()).await;

    // Batu nisan: jadwal terhapus > 90 hari (dipangkas), terhapus baru & tak terhapus (tetap).
    let (store_id, _) = t
        .store(&uniq("ITZ").to_uppercase(), false, true, None)
        .await;
    let mut plans = Vec::new();
    for (date, deleted_ago) in [
        ("2030-01-01", Some(120)),
        ("2030-01-02", Some(5)),
        ("2030-01-03", None),
    ] {
        let id: i32 = sqlx::query_scalar(
            r#"INSERT INTO "storeVisitPlan" ("companyId", "userId", "storeId", "visitDate", "createdById", "deletedAt")
               VALUES ($1, $2, $3, $4::date, $5,
                       CASE WHEN $6::int IS NULL THEN NULL
                            ELSE (now() AT TIME ZONE 'utc') - ($6::int * interval '1 day') END)
               RETURNING id"#,
        )
        .bind(fx.id)
        .bind(fx.staff.id)
        .bind(store_id)
        .bind(date)
        .bind(fx.admin.id)
        .bind(deleted_ago)
        .fetch_one(app.db())
        .await
        .unwrap();
        plans.push(id);
    }

    let result = mz_audit::prune_expired(app.db(), None, Some(2), None)
        .await
        .unwrap();
    assert!(result.audit_logs >= 5, "{result:?}");
    assert!(result.store_visit_plan >= 1, "{result:?}");

    let remaining: Vec<String> = sqlx::query_scalar(
        r#"SELECT uuid::text FROM "auditLog" WHERE "entityLabel" = $1 ORDER BY "createdAt" DESC"#,
    )
    .bind(&p)
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(remaining, vec![keep2, keep1]);
    let alive: Vec<i32> =
        sqlx::query_scalar(r#"SELECT id FROM "storeVisitPlan" WHERE "storeId" = $1 ORDER BY id"#)
            .bind(store_id)
            .fetch_all(app.db())
            .await
            .unwrap();
    assert_eq!(alive, vec![plans[1], plans[2]]);

    // Pemangkasan retensi (SQL mentah) tidak melahirkan baris audit baru.
    let born: i64 = sqlx::query_scalar(
        r#"SELECT count(*) FROM "auditLog"
            WHERE "entityType" = 'storeVisitPlan' AND "entityId" = $1 AND "createdAt" > $2"#,
    )
    .bind(plans[0])
    .bind(started)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(born, 0);

    // Pekerjaan cron yang sama lewat penjadwal: berjalan tanpa galat dan idempoten.
    let jobs = mz_audit::cron_jobs();
    assert_eq!(jobs.len(), 1);
    mz_jobs::run_now(&app.state, &jobs[0]).await.unwrap();
    let again = mz_audit::prune_expired(app.db(), None, None, None)
        .await
        .unwrap();
    assert_eq!(again.audit_logs, 0);
    assert_eq!(again.store_visit_plan, 0);
}
