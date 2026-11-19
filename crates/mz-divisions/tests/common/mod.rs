//! Fixture bersama uji integrasi mz-divisions (bukan binary test sendiri).
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use mz_core::AppState;
use mz_testkit::{Company, TestApp, TestResponse};
use serde_json::Value;

pub fn build(state: &AppState) -> Router {
    mz_divisions::router(state)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

/// Nyalakan divisi untuk company fixture dan kosongkan divisi lama (soft delete). Idempotent.
pub async fn enable(app: &TestApp, fx: &Company) {
    app.exec(&format!(
        r#"UPDATE company SET "divisionsEnabled" = true, "moduleTimeTrackingEnabled" = true WHERE id = {cid};
           INSERT INTO "companyPlanOverride" ("companyId", divisions) VALUES ({cid}, true)
             ON CONFLICT ("companyId") DO UPDATE SET divisions = true;
           -- Izin bawaan fitur `divisions` (di produksi diisi sinkronisasi registry saat boot).
           INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
             SELECT al.id, 'divisions', f.a::"PermissionAction" FROM "accessLevel" al
             CROSS JOIN (VALUES ('READ'), ('WRITE'), ('DELETE')) AS f(a)
             WHERE al."isSystem" AND (f.a = 'READ' OR al.code IN ('SYSTEM_ADMIN', 'SUPER_ADMIN', 'ADMIN'))
             ON CONFLICT DO NOTHING;
           UPDATE "divisionMember" SET "deletedAt" = NOW() WHERE "companyId" = {cid} AND "deletedAt" IS NULL;
           UPDATE division SET "deletedAt" = NOW() WHERE "companyId" = {cid} AND "deletedAt" IS NULL;"#,
        cid = fx.id
    ))
    .await;
}

/// Buat divisi lewat SQL: `members` = (userId, "LEADER" | "MEMBER"). Mengembalikan (id, uuid).
pub async fn make_division(
    app: &TestApp,
    fx: &Company,
    name: &str,
    members: &[(i32, &str)],
) -> (i32, String) {
    let (id, uuid): (i32, String) = sqlx::query_as(
        r#"INSERT INTO division (name, "companyId", "createdById") VALUES ($1, $2, $3)
           RETURNING id, uuid::text"#,
    )
    .bind(name)
    .bind(fx.id)
    .bind(fx.admin.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    for (uid, role) in members {
        sqlx::query(
            r#"INSERT INTO "divisionMember" ("companyId", "divisionId", "userId", role)
               VALUES ($1, $2, $3, $4::"DivisionMemberRole")"#,
        )
        .bind(fx.id)
        .bind(id)
        .bind(uid)
        .bind(role)
        .execute(app.db())
        .await
        .unwrap();
    }
    (id, uuid)
}

/// Posisi kunci berurutan di teks JSON mentah (urutan kunci Node).
pub fn assert_key_order(text: &str, keys: &[&str]) {
    let mut from = 0;
    for k in keys {
        let needle = format!("\"{k}\":");
        let pos = text[from..].find(&needle).unwrap_or_else(|| {
            panic!("kunci `{k}` tidak ditemukan sesudah posisi {from} di {text}")
        });
        from += pos + needle.len();
    }
}

pub fn assert_code(r: &TestResponse, status: u16, code: &str) {
    r.assert_status(status);
    assert_eq!(r.json["code"], Value::from(code), "{}", r.text());
}
