//! Uji integrasi route admin form kunjungan (`/v1/admin/store-visit-forms/**`) lewat HTTP
//! (gerbang lengkap + layer audit) terhadap Postgres nyata; GCS diganti emulator.
//! Jalankan: `MZ_TEST_DATABASE_URL=... cargo test -p mz-visit-forms --test it_visit_forms_admin`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod admin_submissions;
mod admin_templates;
mod common;

use std::time::Duration;

use mz_testkit::{Company, TestApp};
use serde_json::json;
use tokio::sync::Mutex;

/// Semua test memakai company fixture yang sama: dijalankan berurutan.
static SERIAL: Mutex<()> = Mutex::const_new(());

const REVISERS: &str = "/v1/admin/store-visit-forms/revisers";

async fn setup() -> Option<(TestApp, common::Emulator, Company)> {
    let (app, em) = common::app().await?;
    let c = app.company().await;
    common::open_form_gates(&app, c.id).await;
    Some((app, em, c))
}

/// Kembalikan fixture reviser ke keadaan awal (mode bawaan, daftar kosong).
async fn reset_revisers(app: &TestApp, company_id: i32) {
    app.exec(&format!(
        r#"UPDATE company SET "storeVisitFormReviseMode" = 'allPrivileged' WHERE id = {company_id};
           DELETE FROM "storeVisitFormReviser" WHERE "companyId" = {company_id};"#
    ))
    .await;
}

/// Baris audit ditulis sesudah respons: tunggu sampai jumlahnya cukup.
async fn audit_rows(
    app: &TestApp,
    company_id: i32,
    at_least: usize,
) -> Vec<(String, Option<String>)> {
    for _ in 0..50 {
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            r#"SELECT action::text, "subEntity" FROM "auditLog"
                WHERE "entityType"::text = 'company' AND "entityId" = $1
                  AND ("subEntity" = 'formReviser' OR "changedFields" @> ARRAY['storeVisitFormReviseMode'])
                ORDER BY id"#,
        )
        .bind(company_id)
        .fetch_all(app.db())
        .await
        .unwrap();
        if rows.len() >= at_least {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("baris audit reviser < {at_least}");
}

#[tokio::test]
async fn revisers_gates() {
    let _g = SERIAL.lock().await;
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    // 401 tanpa token.
    app.get(REVISERS).send().await.assert_status(401);
    // MANAGER & STAFF ditolak (`@Roles` tanpa MANAGER, §0.6) — RolesGuard Node = 401.
    for u in [&c.manager, &c.staff] {
        let r = app.get(REVISERS).token(&u.token).send().await;
        r.assert_status(401);
        assert_eq!(r.json["message"], "Access denied: insufficient permissions");
        let r = app
            .put(REVISERS)
            .token(&u.token)
            .json(&json!({ "mode": "allPrivileged", "userIds": [] }))
            .send()
            .await;
        r.assert_status(401);
    }
    // Paket tanpa `form` → 403 PLAN_FEATURE_DISABLED.
    app.exec(&format!(
        r#"UPDATE "companyPlanOverride" SET form = false WHERE "companyId" = {}"#,
        c.id
    ))
    .await;
    let boosted: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "companyPlanBoost" WHERE "companyId" = {} AND enabled AND product::text = 'SALES'"#,
            c.id
        ))
        .await;
    let r = app.get(REVISERS).token(&c.admin.token).send().await;
    if boosted == 0 {
        r.assert_status(403);
        assert_eq!(r.json["code"], "PLAN_FEATURE_DISABLED");
    }
    common::open_form_gates(&app, c.id).await;
}

#[tokio::test]
async fn revisers_validation() {
    let _g = SERIAL.lock().await;
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    let r = app
        .get(&format!("{REVISERS}?q=a&q=b"))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "q must be shorter than or equal to 255 characters",
            "q must be a string"
        ])
    );

    let r = app
        .put(REVISERS)
        .token(&c.admin.token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!([
            "mode must be one of the following values: allPrivileged, selected",
            "each value in userIds must be a UUID",
            "userIds must contain no more than 100 elements",
            "userIds must be an array"
        ])
    );

    let r = app
        .put(REVISERS)
        .token(&c.admin.token)
        .json(&json!({ "mode": "selected", "userIds": [] }))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(
        r.json,
        json!({
            "code": "FORM_REVISER_LIST_EMPTY",
            "message": "At least one reviser is required when mode is selected"
        })
    );

    let stranger = "0b7f1a2c-3d4e-4f5a-8b6c-7d8e9f0a1b2c";
    let r = app
        .put(REVISERS)
        .token(&c.admin.token)
        .json(&json!({ "mode": "selected", "userIds": [stranger] }))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(r.json["code"], "FORM_REVISER_INVALID");
    assert_eq!(r.json["details"], json!({ "unknownUserIds": [stranger] }));

    let r = app
        .put(REVISERS)
        .token(&c.admin.token)
        .json(&json!({ "mode": "selected", "userIds": [c.staff.uuid] }))
        .send()
        .await;
    r.assert_status(422);
    assert_eq!(
        r.json,
        json!({
            "code": "FORM_REVISER_ROLE_INELIGIBLE",
            "message": "Every reviser must hold one of: SYSTEM_ADMIN, SUPER_ADMIN, ADMIN, MANAGER",
            "details": { "userIds": [c.staff.uuid] }
        })
    );
    r.assert_no_sql_error();
}

#[tokio::test]
async fn revisers_replace_and_list() {
    let _g = SERIAL.lock().await;
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    reset_revisers(&app, c.id).await;

    let r = app.get(REVISERS).token(&c.admin.token).send().await;
    r.assert_ok().assert_no_sql_error();
    let d = &r.json["data"];
    assert_eq!(d["mode"], "allPrivileged");
    assert_eq!(d["revisers"], json!([]));
    let candidates = d["candidates"].as_array().unwrap();
    let ids: Vec<&str> = candidates
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&c.manager.uuid.as_str()));
    assert!(ids.contains(&c.admin.uuid.as_str()));
    assert!(!ids.contains(&c.staff.uuid.as_str()));
    let keys: Vec<&String> = candidates[0].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["id", "name", "roleCode", "roleName"]);
    // Urut nama (localeCompare).
    let names: Vec<String> = candidates
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_lowercase())
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);

    // Pencarian `q` (case-insensitive) hanya menyaring candidates.
    let manager_name: String = app
        .scalar(&format!(
            "SELECT name FROM users WHERE id = {}",
            c.manager.id
        ))
        .await;
    let r = app
        .get(&format!(
            "{REVISERS}?q={}",
            manager_name.to_uppercase().replace(' ', "%20")
        ))
        .token(&c.admin.token)
        .send()
        .await;
    r.assert_ok();
    let found: Vec<&str> = r.json["data"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert!(found.contains(&c.manager.uuid.as_str()));

    // REPLACE → selected + manager (duplikat dibuang).
    let r = app
        .put(REVISERS)
        .token(&c.admin.token)
        .json(&json!({ "mode": "selected", "userIds": [c.manager.uuid, c.manager.uuid] }))
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    let d = &r.json["data"];
    assert_eq!(d["mode"], "selected");
    let revisers = d["revisers"].as_array().unwrap();
    assert_eq!(revisers.len(), 1);
    let rv = &revisers[0];
    let keys: Vec<&String> = rv.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "id", "user", "roleCode", "roleName", "eligible", "addedAt", "addedBy"
        ]
    );
    assert_eq!(rv["user"]["id"], json!(c.manager.uuid));
    assert_eq!(rv["roleCode"], "MANAGER");
    assert_eq!(rv["eligible"], true);
    assert!(rv["addedAt"].is_i64());
    assert_eq!(rv["addedBy"]["id"], json!(c.admin.uuid));
    let first_id = rv["id"].clone();

    let mode: String = app
        .scalar(&format!(
            r#"SELECT "storeVisitFormReviseMode"::text FROM company WHERE id = {}"#,
            c.id
        ))
        .await;
    assert_eq!(mode, "selected");

    // Audit: update company (mode) + keanggotaan formReviser.
    let rows = audit_rows(&app, c.id, 2).await;
    assert!(
        rows.iter().any(|(a, s)| a == "update" && s.is_none()),
        "{rows:?}"
    );
    assert!(
        rows.iter()
            .any(|(_, s)| s.as_deref() == Some("formReviser")),
        "{rows:?}"
    );

    // Idempoten: kirim ulang → baris yang bertahan tidak ditulis ulang.
    let r = app
        .put(REVISERS)
        .token(&c.admin.token)
        .json(&json!({ "mode": "selected", "userIds": [c.manager.uuid, c.admin.uuid] }))
        .send()
        .await;
    r.assert_ok();
    let revisers = r.json["data"]["revisers"].as_array().unwrap().clone();
    assert_eq!(revisers.len(), 2);
    assert_eq!(revisers[0]["id"], first_id);

    // allPrivileged + daftar sama → daftar tetap; [] → dicabut (soft delete).
    let r = app
        .put(REVISERS)
        .token(&c.admin.token)
        .json(&json!({ "mode": "allPrivileged", "userIds": [c.manager.uuid, c.admin.uuid] }))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.json["data"]["mode"], "allPrivileged");
    assert_eq!(r.json["data"]["revisers"].as_array().unwrap().len(), 2);
    let r = app
        .put(REVISERS)
        .token(&c.admin.token)
        .json(&json!({ "mode": "allPrivileged", "userIds": [] }))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.json["data"]["revisers"], json!([]));
    let tombstones: i64 = app
        .scalar(&format!(
            r#"SELECT count(*) FROM "storeVisitFormReviser" WHERE "companyId" = {} AND "deletedAt" IS NOT NULL"#,
            c.id
        ))
        .await;
    assert_eq!(tombstones, 2);

    reset_revisers(&app, c.id).await;
}

/// `canRevise` terhadap DB: mode selected + daftar.
#[tokio::test]
async fn revise_policy_against_db() {
    use mz_visit_forms::revise_policy::{ReviseReason, can_revise};
    let _g = SERIAL.lock().await;
    let Some((app, _em, c)) = setup().await else {
        return;
    };
    reset_revisers(&app, c.id).await;
    let mut conn = app.db().acquire().await.unwrap();
    let d = can_revise(&mut conn, c.manager.id, c.id).await.unwrap();
    assert_eq!((d.allowed, d.reason), (true, ReviseReason::ModeAll));
    let d = can_revise(&mut conn, c.staff.id, c.id).await.unwrap();
    assert_eq!((d.allowed, d.reason), (false, ReviseReason::RoleIneligible));
    let d = can_revise(&mut conn, c.manager.id, c.id + 100_000)
        .await
        .unwrap();
    assert_eq!(d.reason, ReviseReason::NotMember);

    app.exec(&format!(
        r#"UPDATE company SET "storeVisitFormReviseMode" = 'selected' WHERE id = {0};
           INSERT INTO "storeVisitFormReviser" ("companyId", "userId", "createdById")
           VALUES ({0}, {1}, {1});"#,
        c.id, c.admin.id
    ))
    .await;
    let d = can_revise(&mut conn, c.admin.id, c.id).await.unwrap();
    assert_eq!((d.allowed, d.reason), (true, ReviseReason::Listed));
    let d = can_revise(&mut conn, c.manager.id, c.id).await.unwrap();
    assert_eq!((d.allowed, d.reason), (false, ReviseReason::NotListed));
    let d = can_revise(&mut conn, c.super_admin.id, c.id).await.unwrap();
    assert_eq!(d.reason, ReviseReason::NotListed);
    drop(conn);
    reset_revisers(&app, c.id).await;
}
