//! Fixture bersama uji integrasi `mz-leave-config` (bukan binary test sendiri). Tiap uji membuat
//! company BARU (admin + manager + staff) sehingga uji boleh jalan paralel dan tidak menyentuh
//! company fixture template.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::Router;
use mz_core::AppState;
use mz_testkit::{TestApp, TestResponse};
use serde_json::Value;

pub fn build(state: &AppState) -> Router {
    mz_leave_config::router(state).with_state(state.clone())
}

pub struct U {
    pub id: i32,
    pub token: String,
    pub name: String,
    pub username: String,
}

pub struct Fx {
    pub id: i32,
    pub admin: U,
    pub manager: U,
    pub staff: U,
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// User aktif di company `company_id` dengan role bawaan `role` (kode `accessLevel`).
pub async fn add_user(app: &TestApp, company_id: i32, role: &str, name: &str) -> U {
    let username = format!("lc-{}@test.local", uniq());
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO users (username, password, name, status)
               VALUES ('{username}', 'x', '{name}', 1) RETURNING id"#
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "userCompany" ("userId", "companyId", status, "idleTimeInSecond", "isDefault")
             VALUES ({id}, {company_id}, 1, 180, true);
           INSERT INTO "userAccessLevel" ("userId", "accessLevelId", "companyId", status)
             VALUES ({id}, (SELECT id FROM "accessLevel" WHERE code = '{role}' AND "isSystem" ORDER BY id LIMIT 1),
                     {company_id}, 1);"#
    ))
    .await;
    U {
        id,
        token: app.token(id),
        name: name.to_owned(),
        username,
    }
}

/// Karyawan biasa (anggota company, tanpa peran khusus) untuk target saldo/override/tier.
pub async fn employee(app: &TestApp, company_id: i32, name: &str) -> i32 {
    add_user(app, company_id, "STAFF", name).await.id
}

pub async fn fixture(app: &TestApp) -> Fx {
    let other = app.other_company_admin().await;
    app.exec(
        r#"-- Izin bawaan fitur (di produksi diisi sinkronisasi registry saat boot).
           INSERT INTO "rolePermission" ("accessLevelId", "featureKey", action)
             SELECT al.id, f.k, f.a::"PermissionAction" FROM "accessLevel" al
             CROSS JOIN (VALUES ('leave-config', 'READ'), ('leave-config', 'WRITE'), ('leave-config', 'DELETE'),
                                ('leaves', 'READ'), ('leaves', 'WRITE'), ('leaves', 'DELETE')) AS f(k, a)
             WHERE al."isSystem" AND al.code IN ('SYSTEM_ADMIN', 'SUPER_ADMIN', 'ADMIN', 'MANAGER')
                OR (al."isSystem" AND al.code = 'STAFF' AND f.a = 'READ')
             ON CONFLICT DO NOTHING;"#,
    )
    .await;
    let admin = U {
        id: other.admin.id,
        token: other.admin.token.clone(),
        name: format!("Admin {}", other.id),
        username: other.admin.username.clone(),
    };
    let name: String = app
        .scalar(&format!("SELECT name FROM users WHERE id = {}", admin.id))
        .await;
    let admin = U { name, ..admin };
    let manager = add_user(app, other.id, "MANAGER", &format!("Manager {}", uniq())).await;
    let staff = add_user(app, other.id, "STAFF", &format!("Staff {}", uniq())).await;
    Fx {
        id: other.id,
        admin,
        manager,
        staff,
    }
}

/// Nama kunci objek JSON dalam urutan teks (urutan kunci Node).
pub fn keys(v: &Value) -> Vec<String> {
    v.as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

pub fn assert_keys(v: &Value, expected: &[&str]) {
    assert_eq!(keys(v), expected, "urutan/kunci tidak sama: {v}");
}

/// `message` galat Nest (string atau array) sebagai daftar.
pub fn messages(r: &TestResponse) -> Vec<String> {
    match &r.json["message"] {
        Value::Array(a) => a
            .iter()
            .filter_map(|m| m.as_str().map(str::to_owned))
            .collect(),
        Value::String(s) => vec![s.clone()],
        _ => Vec::new(),
    }
}
