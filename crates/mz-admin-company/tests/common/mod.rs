//! Helper bersama uji integrasi `mz-admin-company`.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::type_complexity
)]

use axum::Router;
use mz_core::AppState;
use mz_testkit::TestApp;
use serde_json::{Value, json};

pub fn build(state: &AppState) -> Router {
    mz_admin_company::router(state)
        .with_state(state.clone())
        .layer(mz_audit::layer())
}

pub fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Operator aktif dengan role tertentu → (id, token).
pub async fn operator(app: &TestApp, role: &str) -> (i32, String) {
    let u = uniq();
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO operators (username, password, name, status, role)
               VALUES ('op{u}', 'x', 'IT Op {role}', 1, '{role}'::"OperatorRole") RETURNING id"#
        ))
        .await;
    (
        id,
        app.token_with(json!({ "username": format!("op{u}"), "sub": id })),
    )
}

pub struct Fx {
    pub id: i32,
    pub uuid: String,
    pub name: String,
    pub owner_id: i32,
    pub owner_uuid: String,
}

/// User aktif + keanggotaan + role sistem `code` di company.
pub async fn member(app: &TestApp, company: i32, code: &str) -> (i32, String) {
    let u = uniq();
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO users (username, password, name, status)
               VALUES ('it-{u}@example.com', 'x', 'Member {u}', 1) RETURNING id"#
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "userCompany" ("userId", "companyId", status, "idleTimeInSecond", "isDefault")
           VALUES ({id}, {company}, 1, 180, true);
           INSERT INTO "userAccessLevel" ("userId", "companyId", "accessLevelId", status)
           VALUES ({id}, {company}, (SELECT id FROM "accessLevel" WHERE "isSystem" = true AND code = '{code}'
                                     AND "companyId" IS NULL ORDER BY id LIMIT 1), 1);"#
    ))
    .await;
    let uuid: String = app
        .scalar(&format!("SELECT uuid::text FROM users WHERE id = {id}"))
        .await;
    (id, uuid)
}

/// Company baru (status 1) + owner SUPER_ADMIN + langganan `sub_sql` (kolom tambahan).
pub async fn company(app: &TestApp, sub_status: &str, billing: &str, tier: i32) -> Fx {
    let u = uniq();
    let name = format!("it-adm-{u}");
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO company (name, status, "timeZone") VALUES ('{name}', 1, 'Asia/Jakarta') RETURNING id"#
        ))
        .await;
    let uuid: String = app
        .scalar(&format!("SELECT uuid::text FROM company WHERE id = {id}"))
        .await;
    let (owner_id, owner_uuid) = member(app, id, "SUPER_ADMIN").await;
    app.exec(&format!(
        r#"UPDATE company SET "ownerUserId" = {owner_id} WHERE id = {id};
           INSERT INTO subscriptions ("companyId", "pricingTierId", "startDate", "endDate", status, "maxUsers",
                                      "billingSource", "pricePerUser")
           VALUES ({id}, {tier}, NOW() - interval '10 day', NOW() + interval '20 day',
                   '{sub_status}'::"SubscriptionStatus", 10, '{billing}'::"SubscriptionBillingSource",
                   (SELECT "pricePerUserUsd" FROM "pricingTiers" WHERE id = {tier}));"#
    ))
    .await;
    Fx {
        id,
        uuid,
        name,
        owner_id,
        owner_uuid,
    }
}

/// `[action, targetType, targetUuid, companyId, before, after, reason]` operator.
pub async fn op_audit(app: &TestApp, op: i32) -> Vec<Value> {
    let rows: Vec<(
        String,
        String,
        Option<String>,
        Option<i32>,
        Option<Value>,
        Option<Value>,
        Option<String>,
    )> = sqlx::query_as(
        r#"SELECT action, "targetType", "targetUuid"::text, "companyId", before, after, reason
               FROM "operatorAuditLog" WHERE "operatorId" = $1 ORDER BY id"#,
    )
    .bind(op)
    .fetch_all(app.db())
    .await
    .unwrap();
    rows.into_iter()
        .map(|(a, t, u, c, b, af, r)| json!([a, t, u, c, b, af, r]))
        .collect()
}

/// Jumlah baris `auditLog` company (entitas `company`) yang ditulis atas nama operator.
pub async fn company_audit_by_operator(app: &TestApp, company: i32) -> i64 {
    // Layer audit menulis sesudah respons (tokio::spawn): tunggu sebentar.
    for _ in 0..40 {
        let n: i64 = app
            .scalar(&format!(
                r#"SELECT COUNT(*) FROM "auditLog" WHERE "companyId" = {company}
                     AND "actorName" LIKE 'Timemoz Support (%'"#
            ))
            .await;
        if n > 0 {
            return n;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    0
}
